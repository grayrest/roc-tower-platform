import pf.Req
import pf.Res
import pf.Cofx
import pf.JsonBool
import Api
import Auth
import Db

## Articles: create, get, update, delete, list, feed, favorite, unfavorite.
##
## Reads are REUSABLE coeffects (`bounds`, `filters`, `loaded_article!`,
## `owned_article!`) composed through the Cofx combinators; each route binds
## the small coeffects it needs with `withN` — slots run left to right, so
## 401 (viewer) precedes 404 (load) precedes 422 (body validation).
Articles := [].{
    ## A new article as POSTed. Validation lives in the `new_article!`
    ## coeffect; the handler sees this shape with fields already checked.
    NewArticle : {
        title : Str,
        description : Str,
        body : Str,
        tagList : Try(List(Str), [Missing]),
    }

    ## A partial article update as PUT — every field optional; a present
    ## tagList REPLACES the tags (`[]` clears), absent preserves.
    ArticlePatch : {
        title : Try(Str, [Missing]),
        description : Try(Str, [Missing]),
        body : Try(Str, [Missing]),
        tagList : Try(List(Str), [Missing]),
    }

    routes = |{ prefix, route }| [
        route("POST", prefix, Articles.create_h!),
        route("GET", "${prefix}/feed", Articles.feed_h!),
        route("GET", "${prefix}/{slug}", Articles.get_h!),
        route("PUT", "${prefix}/{slug}", Articles.update_h!),
        route("DELETE", "${prefix}/{slug}", Articles.delete_h!),
        route("GET", prefix, Articles.list_h!),
        route("POST", "${prefix}/{slug}/favorite", Articles.favorite_h!),
        route("DELETE", "${prefix}/{slug}/favorite", Articles.unfavorite_h!),
    ]

    ## The author profile a brand-new article/comment shows its creator:
    ## you don't follow yourself.
    self_profile : Auth.User -> Db.Profile
    self_profile = |user| {
        username: user.username,
        bio: user.bio,
        image: user.image,
        following: JsonBool.from(False),
    }

    ## Coeffect: pagination bounds off the query string — limit 1..100
    ## (default 20), offset >= 0 (default 0), junk falls back. Pure and
    ## shared by list and feed.
    bounds = {
        limit: Req.query_or("limit", "20"),
        offset: Req.query_or("offset", "0"),
    }.Cofx
        ->Cofx.map(|raw| clamp_bounds(raw.limit, raw.offset))

    ## Coeffect: the list filters, straight off the query string ("" means
    ## unfiltered). Pure.
    filters = {
        tag: Req.query_or("tag", ""),
        author: Req.query_or("author", ""),
        favorited: Req.query_or("favorited", ""),
    }.Cofx

    ## Coeffect: the article at {slug} with its row id and author, as the
    ## (possibly anonymous) viewer sees it — the shared FULL read; 404 on a
    ## miss. Prefer `Db.load_article_meta!`-based coeffects when the
    ## response doesn't need the article body.
    loaded_article! = Req.param("slug")
        ->Cofx.and_then(|slug, req| {
            viewer = Auth.optional_user!(req)?
            Db.load_article!(req.ctx, Db.of(req), Auth.viewer_id(viewer), slug)
        })

    ## Coeffect: the article at {slug}, which the AUTHENTICATED viewer must
    ## own — 401 anonymous, 404 unknown, 403 not-your-article. Full read,
    ## for the update flow (the response echoes the merged article).
    owned_article! = Req.param("slug")
        ->Cofx.and_then(|slug, req| {
            user = Auth.current_user!(req)?
            loaded = Db.load_article!(req.ctx, Db.of(req), user.id, slug)?
            if loaded.author_id == user.id {
                Ok(loaded)
            } else {
                Err(Fail(403.U16, "article", "forbidden"))
            }
        })

    ## Coeffect: the same ownership gate, but via the LIGHT meta read —
    ## DELETE needs only the row id.
    owned_article_id! = Req.param("slug")
        ->Cofx.and_then(|slug, req| {
            user = Auth.current_user!(req)?
            meta = Db.load_article_meta!(req.ctx, Db.of(req), slug)?
            if meta.author_id == user.id {
                Ok(meta.id)
            } else {
                Err(Fail(403.U16, "article", "forbidden"))
            }
        })

    ## Coeffect: the decoded, validated POST body. Pure.
    new_article! = |req| {
        body : { article : Articles.NewArticle }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid article body")
        Ok({
            title: Api.nonblank("title", body.article.title)?,
            description: Api.nonblank("description", body.article.description)?,
            body: Api.nonblank("body", body.article.body)?,
            tagList: body.article.tagList,
        })
    }

    ## Coeffect: the decoded PUT patch, present fields validated non-blank.
    ## Pure.
    article_patch! = |req| {
        body : { article : Articles.ArticlePatch }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid article body")
        Ok({
            title: Api.nonblank_opt("title", body.article.title)?,
            description: Api.nonblank_opt("description", body.article.description)?,
            body: Api.nonblank_opt("body", body.article.body)?,
            tagList: body.article.tagList,
        })
    }

    ## `create` bound to its coeffects, boxed for the route table.
    create_h! = Cofx.bind4(Auth.current_user!, Articles.new_article!, Req.reserved_id!, Req.now!, Articles.create)

    ## PURE handler: INSERT the article and its tags in ONE transaction
    ## (any failure rolls the whole group back), answer the article as the
    ## author sees it.
    create : Auth.User, Articles.NewArticle, I64, U64 -> Res.Response
    create = |author, article, id, now| {
        tags = match article.tagList {
            Ok(list) => list
            Err(Missing) => []
        }
        slug = Api.slugify(article.title, id)
        stamp = Api.iso8601(now)
        base = Res.fx(DbWrite({
            db: Db.db_path,
            sql: "INSERT INTO articles (id, slug, title, description, body, author_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params: [
                Integer(id),
                Text(slug),
                Text(article.title),
                Text(article.description),
                Text(article.body),
                Integer(author.id),
                Text(stamp),
                Text(stamp),
            ],
        }))
        with_tags = tags.fold(base, |fx, tag| {
            fx.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO article_tags (article_id, tag) VALUES (?, ?)",
                params: [Integer(id), Text(tag)],
            }))
        })
        with_tags.json(201, {
            article: {
                slug: slug,
                title: article.title,
                description: article.description,
                body: article.body,
                tagList: tags,
                createdAt: stamp,
                updatedAt: stamp,
                favorited: JsonBool.from(False),
                favoritesCount: 0.I64,
                author: Articles.self_profile(author),
            },
        })
    }

    ## `get` bound to the typed `loaded_article!` read.
    get_h! = Cofx.bind1(Articles.loaded_article!, Articles.get)

    ## PURE handler: serialize the typed article as `{article}` in Roc.
    ## Cacheable: anonymous GETs hit the response cache (authenticated requests
    ## carry `authorization` and bypass it).
    get : { id : I64, author_id : I64, article : Db.Article } -> Res.Response
    get = |loaded| Res.json(200, { article: loaded.article })

    ## `update` bound to its coeffects (owner gate, patch, instant).
    update_h! = Cofx.bind3(Articles.owned_article!, Articles.article_patch!, Req.now!, Articles.update)

    ## PURE handler: merge the patch, re-slug when the title changed (the
    ## id keeps it unique), UPDATE row and tags in one Must group, answer
    ## the merged article.
    update : { id : I64, author_id : I64, article : Db.Article }, Articles.ArticlePatch, U64 -> Res.Response
    update = |owned, changes, now| {
        existing = owned.article
        title = or_keep(changes.title, existing.title)
        description = or_keep(changes.description, existing.description)
        body = or_keep(changes.body, existing.body)
        slug = match changes.title {
            Ok(new_title) => Api.slugify(new_title, owned.id)
            Err(Missing) => existing.slug
        }
        stamp = Api.iso8601(now)
        base = Res.fx(DbWrite({
            db: Db.db_path,
            sql: "UPDATE articles SET slug = ?, title = ?, description = ?, body = ?, updated_at = ? WHERE id = ?",
            params: [Text(slug), Text(title), Text(description), Text(body), Text(stamp), Integer(owned.id)],
        }))
        # A present tagList REPLACES the tags (an empty list clears them);
        # absent leaves them untouched. All in the same transaction as the
        # row update.
        with_tags = match changes.tagList {
            Err(Missing) => base
            Ok(tags) => tags.fold(
                base.fx(DbWrite({
                    db: Db.db_path,
                    sql: "DELETE FROM article_tags WHERE article_id = ?",
                    params: [Integer(owned.id)],
                })),
                |fx, tag| fx.fx(DbWrite({
                    db: Db.db_path,
                    sql: "INSERT OR IGNORE INTO article_tags (article_id, tag) VALUES (?, ?)",
                    params: [Integer(owned.id), Text(tag)],
                })),
            )
        }
        tag_list = match changes.tagList {
            Err(Missing) => existing.tagList
            Ok(tags) => tags
        }
        with_tags.json(200, {
            article: { ..existing, slug: slug, title: title, description: description, body: body, updatedAt: stamp, tagList: tag_list },
        })
    }

    ## `delete` bound to the light ownership gate.
    delete_h! = Cofx.bind1(Articles.owned_article_id!, Articles.delete)

    ## PURE handler: delete the article and its dependents in ONE
    ## transaction (comments, favorites, tags, then the row).
    delete : I64 -> Res.Response
    delete = |article_id| {
        Res.fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM comments WHERE article_id = ?", params: [Integer(article_id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM favorites WHERE article_id = ?", params: [Integer(article_id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM article_tags WHERE article_id = ?", params: [Integer(article_id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM articles WHERE id = ?", params: [Integer(article_id)] }))
            .no_content()
    }

    ## Coeffect: the filtered, paginated global article list + count. `filters`
    ## builds the query shape once; the Db call differs by mode.
    list_filters = { filters: Articles.filters, bounds: Articles.bounds }.Cofx

    list_page! = Articles.list_filters
        ->Cofx.and_then(|input, req| {
            viewer = Auth.optional_user!(req)?
            Db.list_articles!(req.ctx, Db.of(req), Auth.viewer_id(viewer), Articles.list_args(input))
        })

    ## PURE: the filters+bounds coeffect value to the Db list-args record.
    list_args = |input| {
        tag: input.filters.tag,
        author: input.filters.author,
        favorited: input.filters.favorited,
        limit: input.bounds.limit,
        offset: input.bounds.offset,
    }

    ## Coeffect: the authenticated viewer's feed page + count.
    feed_page! = Articles.bounds
        ->Cofx.and_then(|page_bounds, req| {
            user = Auth.current_user!(req)?
            Db.feed_articles!(req.ctx, Db.of(req), user.id, page_bounds.limit, page_bounds.offset)
        })

    ## `page` bound to each of its two sources (global list, viewer's feed).
    list_h! = Cofx.bind1(Articles.list_page!, Articles.page)

    feed_h! = Cofx.bind1(Articles.feed_page!, Articles.page)

    ## PURE handler: serialize the typed page envelope (summaries OMIT the body
    ## the spec forbids in list items) as `{articles, articlesCount}` in Roc.
    page : { articles : List(Db.ArticleSummary), articlesCount : I64 } -> Res.Response
    page = |data| Res.json(200, data)

    ## `favorite` bound to its coeffects.
    favorite_h! = Cofx.bind2(Auth.current_user!, Articles.loaded_article!, Articles.favorite)

    ## PURE handler: record the favorite (idempotent), answer the article
    ## with the flag on and the count the write will produce.
    favorite : Auth.User, { id : I64, author_id : I64, article : Db.Article } -> Res.Response
    favorite = |user, loaded| {
        already = JsonBool.truth(loaded.article.favorited)
        count : I64
        count = if already loaded.article.favoritesCount else loaded.article.favoritesCount + 1
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO favorites (user_id, article_id) VALUES (?, ?)",
                params: [Integer(user.id), Integer(loaded.id)],
            }))
            .json(200, { article: { ..loaded.article, favorited: JsonBool.from(True), favoritesCount: count } })
    }

    ## `unfavorite` bound to its coeffects.
    unfavorite_h! = Cofx.bind2(Auth.current_user!, Articles.loaded_article!, Articles.unfavorite)

    ## PURE handler: remove the favorite (idempotent), mirror of favorite.
    unfavorite : Auth.User, { id : I64, author_id : I64, article : Db.Article } -> Res.Response
    unfavorite = |user, loaded| {
        already = JsonBool.truth(loaded.article.favorited)
        count : I64
        count = if already loaded.article.favoritesCount - 1 else loaded.article.favoritesCount
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "DELETE FROM favorites WHERE user_id = ? AND article_id = ?",
                params: [Integer(user.id), Integer(loaded.id)],
            }))
            .json(200, { article: { ..loaded.article, favorited: JsonBool.from(False), favoritesCount: count } })
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: clamp raw limit/offset strings to sane pagination bounds.
clamp_bounds : Str, Str -> { limit : I64, offset : I64 }
clamp_bounds = |limit_raw, offset_raw| {
    limit = match I64.from_str(limit_raw) {
        Ok(n) => if n < 1 1 else if n > 100 100 else n
        Err(_) => 20
    }
    offset = match I64.from_str(offset_raw) {
        Ok(n) => if n < 0 0 else n
        Err(_) => 0
    }
    { limit: limit, offset: offset }
}

## Internal: a partial-update field — the sent value, or the current one.
or_keep : Try(Str, [Missing]), Str -> Str
or_keep = |field, existing| match field {
    Ok(value) => value
    Err(Missing) => existing
}

## clamp_bounds turns junk and out-of-range values into the defaults
expect clamp_bounds("junk", "-3") == { limit: 20, offset: 0 }

## clamp_bounds passes in-range values through
expect clamp_bounds("5", "10") == { limit: 5, offset: 10 }

## clamp_bounds caps the limit at 100
expect clamp_bounds("5000", "0") == { limit: 100, offset: 0 }
