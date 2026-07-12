import pf.Req
import pf.Res
import pf.Cofx
import pf.NullableStr
import pf.JsonBool
import Api
import Auth
import Db

## Articles: create, get, update, delete, list, feed, favorite, unfavorite.
Articles := [].{
    routes = |{}| [
        { method: "POST", path: "/api/articles", handler: Box.box(Cofx.with1(Articles.create_input!, Articles.create)) },
        { method: "GET", path: "/api/articles/feed", handler: Box.box(Cofx.with1(Articles.feed_input!, Articles.page)) },
        { method: "GET", path: "/api/articles/{slug}", handler: Box.box(Cofx.with1(Articles.article!, Articles.get)) },
        { method: "PUT", path: "/api/articles/{slug}", handler: Box.box(Cofx.with1(Articles.update_input!, Articles.update)) },
        { method: "DELETE", path: "/api/articles/{slug}", handler: Box.box(Cofx.with1(Articles.owned_article!, Articles.delete)) },
        { method: "GET", path: "/api/articles", handler: Box.box(Cofx.with1(Articles.list_input!, Articles.page)) },
        { method: "POST", path: "/api/articles/{slug}/favorite", handler: Box.box(Cofx.with1(Articles.favorite_input!, Articles.favorite)) },
        { method: "DELETE", path: "/api/articles/{slug}/favorite", handler: Box.box(Cofx.with1(Articles.favorite_input!, Articles.unfavorite)) },
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

    ## Coeffect: everything creation needs — author, decoded body, a
    ## reserved article id (slug uniqueness), and the request instant.
    create_input! = |req| {
        author = Auth.current_user!(req)?
        body : { article : { title : Str, description : Str, body : Str, tagList : Try(List(Str), [Missing]) } }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid article body")
        title = Api.nonblank("title", body.article.title)?
        description = Api.nonblank("description", body.article.description)?
        text = Api.nonblank("body", body.article.body)?
        id = Req.reserved_id!(req)?
        now = Req.now!(req)?
        Ok({
            author: author,
            title: title,
            description: description,
            text: text,
            tagList: body.article.tagList,
            id: id,
            now: now,
        })
    }

    ## PURE handler: INSERT the article and its tags in ONE transaction
    ## (any failure rolls the whole group back), answer the article as the
    ## author sees it.
    create = |input| {
        tags = match input.tagList {
            Ok(list) => list
            Err(Missing) => []
        }
        slug = Api.slugify(input.title, input.id)
        stamp = Api.iso8601(input.now)
        base = Res.fx(DbWrite({
            db: Db.db_path,
            sql: "INSERT INTO articles (id, slug, title, description, body, author_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params: [
                Integer(input.id),
                Text(slug),
                Text(input.title),
                Text(input.description),
                Text(input.text),
                Integer(input.author.id),
                Text(stamp),
                Text(stamp),
            ],
        }))
        with_tags = tags.fold(base, |fx, tag| {
            fx.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO article_tags (article_id, tag) VALUES (?, ?)",
                params: [Integer(input.id), Text(tag)],
            }))
        })
        with_tags.json(201, {
            article: {
                slug: slug,
                title: input.title,
                description: input.description,
                body: input.text,
                tagList: tags,
                createdAt: stamp,
                updatedAt: stamp,
                favorited: JsonBool.from(False),
                favoritesCount: 0.I64,
                author: Articles.self_profile(input.author),
            },
        })
    }

    ## Coeffect: the article at {slug} as the (possibly anonymous) viewer
    ## sees it; 404 on a miss.
    article! = |req| {
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        viewer = Auth.optional_user!(req)?
        loaded = Db.load_article!(Box.unbox(req.model).db, Auth.viewer_id(viewer), slug)?
        Ok(loaded.article)
    }

    ## PURE handler: the article envelope.
    get : Db.Article -> Res.Response
    get = |article| Res.json(200, { article: article })

    ## Coeffect: the article at {slug}, loaded as an AUTHENTICATED viewer
    ## who must be its author — the update/delete authorization, expressed
    ## once as a resolver: 401 anonymous, 404 unknown, 403 not-your-article.
    owned_article! = |req| {
        user = Auth.current_user!(req)?
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        loaded = Db.load_article!(Box.unbox(req.model).db, user.id, slug)?
        if loaded.author_id == user.id {
            Ok({ user: user, id: loaded.id, article: loaded.article })
        } else {
            Err(Api.forbidden("article"))
        }
    }

    ## Coeffect: the owned article plus the decoded partial update and the
    ## request instant.
    update_input! = |req| {
        owned = Articles.owned_article!(req)?
        body : {
            article : {
                title : Try(Str, [Missing]),
                description : Try(Str, [Missing]),
                body : Try(Str, [Missing]),
                tagList : Try(List(Str), [Missing]),
            },
        }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid article body")
        _ = match body.article.title {
            Ok(value) => Api.nonblank("title", value)?
            Err(Missing) => ""
        }
        _ = match body.article.description {
            Ok(value) => Api.nonblank("description", value)?
            Err(Missing) => ""
        }
        _ = match body.article.body {
            Ok(value) => Api.nonblank("body", value)?
            Err(Missing) => ""
        }
        now = Req.now!(req)?
        Ok({ owned: owned, changes: body.article, now: now })
    }

    ## A partial-update field: the sent value, or the current one.
    or_keep : Try(Str, [Missing]), Str -> Str
    or_keep = |field, existing| match field {
        Ok(value) => value
        Err(Missing) => existing
    }

    ## PURE handler: merge changes, re-slug when the title changed (the id
    ## keeps it unique), UPDATE in one Must, answer the merged article.
    update = |input| {
        existing = input.owned.article
        title = Articles.or_keep(input.changes.title, existing.title)
        description = Articles.or_keep(input.changes.description, existing.description)
        body = Articles.or_keep(input.changes.body, existing.body)
        slug = match input.changes.title {
            Ok(new_title) => Api.slugify(new_title, input.owned.id)
            Err(Missing) => existing.slug
        }
        stamp = Api.iso8601(input.now)
        base = Res.fx(DbWrite({
            db: Db.db_path,
            sql: "UPDATE articles SET slug = ?, title = ?, description = ?, body = ?, updated_at = ? WHERE id = ?",
            params: [Text(slug), Text(title), Text(description), Text(body), Text(stamp), Integer(input.owned.id)],
        }))
        # A present tagList REPLACES the tags (an empty list clears them);
        # absent leaves them untouched. All in the same transaction as the
        # row update. (Pinned: the handler's input record is otherwise open.)
        new_tags : Try(List(Str), [Missing])
        new_tags = input.changes.tagList
        with_tags = match new_tags {
            Err(Missing) => base
            Ok(tags) => tags.fold(
                base.fx(DbWrite({
                    db: Db.db_path,
                    sql: "DELETE FROM article_tags WHERE article_id = ?",
                    params: [Integer(input.owned.id)],
                })),
                |fx, tag| fx.fx(DbWrite({
                    db: Db.db_path,
                    sql: "INSERT OR IGNORE INTO article_tags (article_id, tag) VALUES (?, ?)",
                    params: [Integer(input.owned.id), Text(tag)],
                })),
            )
        }
        tag_list = match new_tags {
            Err(Missing) => existing.tagList
            Ok(tags) => tags
        }
        with_tags.json(200, {
            article: { ..existing, slug: slug, title: title, description: description, body: body, updatedAt: stamp, tagList: tag_list },
        })
    }

    ## PURE handler: delete the article and its dependents in ONE
    ## transaction (comments, favorites, tags, then the row).
    delete = |input| {
        Res.fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM comments WHERE article_id = ?", params: [Integer(input.id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM favorites WHERE article_id = ?", params: [Integer(input.id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM article_tags WHERE article_id = ?", params: [Integer(input.id)] }))
            .fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM articles WHERE id = ?", params: [Integer(input.id)] }))
            .no_content()
    }

    ## Pagination bounds shared by list and feed: limit 1..100 (default
    ## 20), offset >= 0 (default 0); junk falls back to the default.
    page_bounds : Str, Str -> { limit : I64, offset : I64 }
    page_bounds = |limit_raw, offset_raw| {
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

    ## Coeffect: the filtered, paginated global article list + count.
    list_input! = |req| {
        viewer = Auth.optional_user!(req)?
        tag = Req.query_or("tag", "")(req)?
        author = Req.query_or("author", "")(req)?
        favorited = Req.query_or("favorited", "")(req)?
        limit_raw = Req.query_or("limit", "20")(req)?
        offset_raw = Req.query_or("offset", "0")(req)?
        bounds = Articles.page_bounds(limit_raw, offset_raw)
        Db.list_articles!(Box.unbox(req.model).db, Auth.viewer_id(viewer), {
            tag: tag,
            author: author,
            favorited: favorited,
            limit: bounds.limit,
            offset: bounds.offset,
        })
    }

    ## Coeffect: the authenticated viewer's feed page + count.
    feed_input! = |req| {
        user = Auth.current_user!(req)?
        limit_raw = Req.query_or("limit", "20")(req)?
        offset_raw = Req.query_or("offset", "0")(req)?
        bounds = Articles.page_bounds(limit_raw, offset_raw)
        Db.feed_articles!(Box.unbox(req.model).db, user.id, bounds.limit, bounds.offset)
    }

    ## PURE handler: a page of articles. The spec requires list items to
    ## OMIT the article body, so full rows shrink to summaries here.
    page : { articles : List(Db.Article), articlesCount : I64 } -> Res.Response
    page = |result| Res.json(200, {
        articles: result.articles.map(|article| Db.to_summary(article)),
        articlesCount: result.articlesCount,
    })

    ## Coeffect: an authenticated viewer plus the article to (un)favorite.
    favorite_input! = |req| {
        user = Auth.current_user!(req)?
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        loaded = Db.load_article!(Box.unbox(req.model).db, user.id, slug)?
        Ok({ user_id: user.id, id: loaded.id, article: loaded.article })
    }

    ## PURE handler: record the favorite (idempotent), answer the article
    ## with the flag on and the count the write will produce.
    favorite = |input| {
        already = JsonBool.truth(input.article.favorited)
        count : I64
        count = if already input.article.favoritesCount else input.article.favoritesCount + 1
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO favorites (user_id, article_id) VALUES (?, ?)",
                params: [Integer(input.user_id), Integer(input.id)],
            }))
            .json(200, { article: { ..input.article, favorited: JsonBool.from(True), favoritesCount: count } })
    }

    ## PURE handler: remove the favorite (idempotent), mirror of favorite.
    unfavorite = |input| {
        already = JsonBool.truth(input.article.favorited)
        count : I64
        count = if already input.article.favoritesCount - 1 else input.article.favoritesCount
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "DELETE FROM favorites WHERE user_id = ? AND article_id = ?",
                params: [Integer(input.user_id), Integer(input.id)],
            }))
            .json(200, { article: { ..input.article, favorited: JsonBool.from(False), favoritesCount: count } })
    }
}

## page_bounds clamps junk and out-of-range values to sane defaults
expect Articles.page_bounds("junk", "-3") == { limit: 20, offset: 0 }

## page_bounds passes through in-range values
expect Articles.page_bounds("5", "10") == { limit: 5, offset: 10 }

## page_bounds caps the limit at 100
expect Articles.page_bounds("5000", "0") == { limit: 100, offset: 0 }
