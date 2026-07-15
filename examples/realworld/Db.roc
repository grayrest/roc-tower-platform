import pf.Host
import pf.Sql
import pf.NullableStr
import pf.JsonBool
import pf.RawJson
import Api
import DbGen

## Schema, queries, and row decoders for the RealWorld data model. Reads run
## inside coeffect resolvers (`Sql.query!` over the streaming cursor); writes
## are built by pure handlers as `DbWrite` effects against the same `db_path`.
Db := [].{
    ## One fixed database file for the whole app; the e2e suite and the
    ## conformance runner clear it before each run.
    db_path = "/tmp/tower-platform-realworld.db"

    ## RealWorld profile object, JSON-shaped (camelCase, nullable bio/image,
    ## boolean following).
    Profile : { username : Str, bio : NullableStr, image : NullableStr, following : JsonBool }

    ## RealWorld article object, JSON-shaped. The row id is carried alongside
    ## (not inside) by `load_article!`, so responses stay spec-shaped.
    Article : {
        slug : Str,
        title : Str,
        description : Str,
        body : Str,
        tagList : List(Str),
        createdAt : Str,
        updatedAt : Str,
        favorited : JsonBool,
        favoritesCount : I64,
        author : Db.Profile,
    }

    ## The database file the request's model names — the standard first step
    ## of a coeffect that reads.
    of = |req| Box.unbox(req.model).db

    ## A NullableStr as a SQL parameter (Val -> TEXT, Null -> NULL).
    to_sql : NullableStr -> Host.SqlValue
    to_sql = |nullable| match nullable {
        Val(text) => Text(text)
        Null => Null
    }

    ## The article SELECT shared three ways: sqlgen splices it into the checked
    ## by-slug reader (`article_by_slug_q`, positional cell reads), and the
    ## dynamic list/feed path interpolates the SAME constant at runtime
    ## (`Sql.query!`, by-name reads). `:viewer` is one named parameter (SQLite
    ## reuses it for the favorited + following flags). Aliases stay PLAIN so the
    ## column names match the by-name decoders; the flag columns come back as
    ## I64 (0/1) and become `JsonBool` in the row mappers.
    ## @fragment article_select
    article_select =
        \\SELECT a.id, a.author_id, a.slug, a.title, a.description, a.body, a.created_at, a.updated_at,
        \\       u.username, u.bio, u.image,
        \\       (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id) AS favorites_count,
        \\       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer) AS favorited,
        \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id) AS following,
        \\       (SELECT GROUP_CONCAT(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)) AS tag_list
        \\FROM articles a JOIN users u ON u.id = a.author_id

    ## GROUP_CONCAT output to a tag list: NULL (no tags) and "" become [].
    split_tags : NullableStr -> List(Str)
    split_tags = |joined| match joined {
        Null => []
        Val(text) => if text.is_empty() [] else text.split_on(",")
    }

    ## Load one article by slug with the viewer's flags; rejects 404 on a
    ## miss. Returns the row id alongside the JSON-shaped article for
    ## author checks, updates, and join-table writes.
    ## @params slug: Str, viewer: I64
    ## @sql one
    article_by_slug_q =
        \\${article_select} WHERE a.slug = :slug

    load_article! = |ctx, db, viewer, slug| {
        row = DbGen.article_by_slug_q!(ctx, db, { slug: slug, viewer: viewer }) ? |err| match err {
            NoRows => Api.not_found("article")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok({ id: row.id, author_id: row.author_id, article: Db.article_from_row(row) })
    }

    ## Map a generated article row (flat, flags as I64) to the JSON-shaped
    ## article: split the tag list, nest the author, flags to JsonBool.
    article_from_row = |row| {
        slug: row.slug,
        title: row.title,
        description: row.description,
        body: row.body,
        tagList: Db.split_tags(row.tag_list),
        createdAt: row.created_at,
        updatedAt: row.updated_at,
        favorited: JsonBool.from(row.favorited != 0),
        favoritesCount: row.favorites_count,
        author: {
            username: row.username,
            bio: row.bio,
            image: row.image,
            following: JsonBool.from(row.following != 0),
        },
    }

    ## The article JSON document, assembled in SQL and reused two ways: sqlgen
    ## splices it into the `@sql json` by-slug reader (patched with `body` for
    ## the full view), and the dynamic list/feed path interpolates the SAME
    ## constant at runtime into a `json_group_array` (body-less summaries).
    ## `:viewer` is one named param (SQLite reuses it for the favorited +
    ## following flags); NULL bio/image become JSON `null`, the 0/1 flags become
    ## real booleans via `json(iif(...))`, and the tags become a JSON array.
    ## @fragment article_json_summary
    article_json_summary =
        \\json_object(
        \\  'slug', a.slug, 'title', a.title, 'description', a.description,
        \\  'tagList', (SELECT json_group_array(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)),
        \\  'createdAt', a.created_at, 'updatedAt', a.updated_at,
        \\  'favorited', json(iif(EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer), 'true', 'false')),
        \\  'favoritesCount', (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id),
        \\  'author', json_object('username', u.username, 'bio', u.bio, 'image', u.image,
        \\    'following', json(iif(EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id), 'true', 'false'))))

    ## GET /articles/{slug}: the full article envelope, `body` patched into the
    ## shared summary, assembled entirely in SQL. Rejects 404 on a miss.
    ## @params slug: Str, viewer: I64
    ## @sql json one
    article_json_q =
        \\SELECT json_object('article', json_patch(${article_json_summary}, json_object('body', a.body)))
        \\FROM articles a JOIN users u ON u.id = a.author_id WHERE a.slug = :slug

    load_article_json! = |ctx, db, viewer, slug| {
        doc = DbGen.article_json_q!(ctx, db, { slug: slug, viewer: viewer }) ? |err| match err {
            NoRows => Api.not_found("article")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok(doc)
    }

    ## The LIGHT article lookup for existence and ownership checks: just the
    ## row id and author, no joins/aggregates. Use `load_article!` only when
    ## the response needs the full JSON-shaped article.
    ## @params slug: Str
    ## @sql one
    article_meta_q =
        \\SELECT id, author_id FROM articles WHERE slug = :slug

    load_article_meta! = |ctx, db, slug| {
        row = DbGen.article_meta_q!(ctx, db, { slug: slug }) ? |err| match err {
            NoRows => Api.not_found("article")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok(row)
    }

    ## List articles with optional tag/author/favorited filters, most recent
    ## first, plus the unpaginated count (the `articlesCount` field).
    ##
    ## Each filter carries TWO spellings of the same predicate: the PAGE
    ## query wants EXISTS (the planner keeps the ORDER BY id DESC scan and
    ## early-exits at LIMIT, probing per row — an IN-list would materialize
    ## every match and pay the SELECT-clause subqueries for all of them);
    ## the COUNT query wants IN (materialize the id-list once instead of
    ## probing all rows).
    list_articles! = |ctx, db, viewer, filters| {
        tag_part =
            if filters.tag.is_empty()
                { page: [], count: [], params: [] }
            else
                {
                    page: ["EXISTS(SELECT 1 FROM article_tags att WHERE att.article_id = a.id AND att.tag = ?)"],
                    count: ["att.tag = ?"],
                    params: [Text(filters.tag)],
                }
        author_part =
            if filters.author.is_empty()
                { page: [], count: [], params: [] }
            else
                { page: ["u.username = ?"], count: ["u.username = ?"], params: [Text(filters.author)] }
        favorited_part =
            if filters.favorited.is_empty()
                { page: [], count: [], params: [] }
            else
                # favorited-by-user is naturally SPARSE (a user favorites
                # dozens of articles, not thousands), so the IN-list is the
                # right plan for the page query too: materialize the few ids
                # instead of probing every article row.
                {
                    page: ["a.id IN (SELECT fv.article_id FROM favorites fv JOIN users fu ON fu.id = fv.user_id WHERE fu.username = ?)"],
                    count: ["a.id IN (SELECT fv.article_id FROM favorites fv JOIN users fu ON fu.id = fv.user_id WHERE fu.username = ?)"],
                    params: [Text(filters.favorited)],
                }
        page_clauses = tag_part.page.concat(author_part.page).concat(favorited_part.page)
        count_clauses = tag_part.count.concat(author_part.count).concat(favorited_part.count)
        where_params = tag_part.params.concat(author_part.params).concat(favorited_part.params)
        page_where = if page_clauses.is_empty() "" else " WHERE ${Str.join_with(page_clauses, " AND ")}"
        count_where = if count_clauses.is_empty() "" else " WHERE ${Str.join_with(count_clauses, " AND ")}"
        # Count spelling by filter shape, cheapest first: a tag-only filter
        # is a pure index-range count over article_tags (the (article_id,
        # tag) PK is unique, so no double counting) — measured ~50x cheaper
        # under concurrency than probing article rows. A tag COMBINED with
        # other filters still drives from the tag index but joins the rest.
        tag_only = !filters.tag.is_empty() and filters.author.is_empty() and filters.favorited.is_empty()
        count_sql =
            if tag_only
                "SELECT COUNT(*) AS n FROM article_tags att${count_where}"
            else if filters.tag.is_empty()
                "SELECT COUNT(*) AS n FROM articles a JOIN users u ON u.id = a.author_id${count_where}"
            else
                "SELECT COUNT(*) AS n FROM article_tags att JOIN articles a ON a.id = att.article_id JOIN users u ON u.id = a.author_id${count_where}"
        run_article_page!(ctx, db, viewer, page_where, count_sql, where_params, filters.limit, filters.offset)
    }

    ## The viewer's feed: articles by followed authors, most recent first.
    ## Same EXISTS-for-page / IN-for-count split as `list_articles!`.
    feed_articles! = |ctx, db, viewer, limit, offset| {
        page_where = " WHERE EXISTS(SELECT 1 FROM follows fo2 WHERE fo2.follower_id = ? AND fo2.followed_id = a.author_id)"
        count_sql = "SELECT COUNT(*) AS n FROM articles a WHERE a.author_id IN (SELECT followed_id FROM follows WHERE follower_id = ?)"
        run_article_page!(ctx, db, viewer, page_where, count_sql, [Integer(viewer)], limit, offset)
    }

    ## Load a profile by username with the viewer's following flag; rejects
    ## 404 on a miss. Returns the user id alongside for follow writes.
    ## @params username: Str, viewer: I64
    ## @sql one
    profile_by_username_q =
        \\SELECT u.id, u.username, u.bio, u.image,
        \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = u.id) AS "following: Bool"
        \\FROM users u WHERE u.username = :username

    load_profile! = |ctx, db, viewer, username| {
        row = DbGen.profile_by_username_q!(ctx, db, { username: username, viewer: viewer }) ? |err| match err {
            NoRows => Api.not_found("profile")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok({
            id: row.id,
            profile: { username: row.username, bio: row.bio, image: row.image, following: row.following },
        })
    }

    ## GET /profiles/{username}: the profile envelope, assembled in SQL.
    ## Rejects 404 on a miss.
    ## @params username: Str, viewer: I64
    ## @sql json one
    profile_json_q =
        \\SELECT json_object('profile', json_object('username', u.username, 'bio', u.bio, 'image', u.image,
        \\  'following', json(iif(EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = u.id), 'true', 'false'))))
        \\FROM users u WHERE u.username = :username

    load_profile_json! = |ctx, db, viewer, username| {
        doc = DbGen.profile_json_q!(ctx, db, { username: username, viewer: viewer }) ? |err| match err {
            NoRows => Api.not_found("profile")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok(doc)
    }

    ## GET /articles/{slug}/comments: the comments envelope, assembled in SQL
    ## (oldest first, with the viewer's following flags). Always one row —
    ## `{"comments":[]}` when the article has none.
    ## @params viewer: I64, article_id: I64
    ## @sql json
    comments_json_q =
        \\SELECT json_object('comments', COALESCE((
        \\    SELECT json_group_array(json_object('id', c.id, 'createdAt', c.created_at, 'updatedAt', c.updated_at, 'body', c.body,
        \\        'author', json_object('username', u.username, 'bio', u.bio, 'image', u.image,
        \\            'following', json(iif(EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = c.author_id), 'true', 'false')))))
        \\    FROM comments c JOIN users u ON u.id = c.author_id WHERE c.article_id = :article_id ORDER BY c.id), json('[]')))

    load_comments_json! = |ctx, db, viewer, article_id| {
        doc = DbGen.comments_json_q!(ctx, db, { viewer: viewer, article_id: article_id })
            ? |DbErr(message)| Api.error(500, "db error: ${message}")
        Ok(doc)
    }

    ## RealWorld comment object, JSON-shaped (nested author profile).
    Comment : { id : I64, createdAt : Str, updatedAt : Str, body : Str, author : Db.Profile }

    ## TYPED comments read: flat rows (author fields alongside), oldest first,
    ## with the viewer's following flag as a real boolean (`: Bool` → JsonBool).
    ## The Roc side nests the author and shapes the envelope.
    ## @params viewer: I64, article_id: I64
    ## @sql
    comments_q =
        \\SELECT c.id, c.created_at, c.updated_at, c.body,
        \\       u.username, u.bio, u.image,
        \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = c.author_id) AS "following: Bool"
        \\FROM comments c JOIN users u ON u.id = c.author_id
        \\WHERE c.article_id = :article_id ORDER BY c.id

    load_comments! = |ctx, db, viewer, article_id| {
        rows = DbGen.comments_q!(ctx, db, { viewer: viewer, article_id: article_id })
            ? |DbErr(message)| Api.error(500, "db error: ${message}")
        Ok(rows.map(Db.comment_from_row))
    }

    ## Map a generated comment row (flat) to the JSON-shaped comment: nest the
    ## author profile.
    comment_from_row = |row| {
        id: row.id,
        createdAt: row.created_at,
        updatedAt: row.updated_at,
        body: row.body,
        author: { username: row.username, bio: row.bio, image: row.image, following: row.following },
    }

    ## One comment's id + author for the delete-comment authorization check;
    ## scoped to the article so a wrong-article id 404s.
    ## @params comment_id: I64, article_id: I64
    ## @sql one
    comment_meta_q =
        \\SELECT c.id, c.author_id FROM comments c WHERE c.id = :comment_id AND c.article_id = :article_id

    load_comment! = |ctx, db, comment_id, article_id| {
        row = DbGen.comment_meta_q!(ctx, db, { comment_id: comment_id, article_id: article_id }) ? |err| match err {
            NoRows => Api.not_found("comment")
            DbErr(message) => Api.error(500, "db error: ${message}")
        }
        Ok(row)
    }

    ## GET /tags: the tags envelope, assembled in SQL (every tag in use,
    ## sorted). Always one row.
    ## @sql json
    tags_json_q =
        \\SELECT json_object('tags', COALESCE((SELECT json_group_array(tag) FROM (SELECT DISTINCT tag FROM article_tags ORDER BY tag)), json('[]')))

    all_tags_json! = |ctx, db| {
        doc = DbGen.tags_json_q!(ctx, db, {}) ? |DbErr(message)| Api.error(500, "db error: ${message}")
        Ok(doc)
    }

    ## TYPED tags read: every tag in use, sorted. The Roc side pulls the one
    ## column out to a `List(Str)` and shapes the `{tags}` envelope.
    ## @sql
    tags_q =
        \\SELECT DISTINCT tag FROM article_tags ORDER BY tag

    all_tags! = |ctx, db| {
        rows = DbGen.tags_q!(ctx, db, {}) ? |DbErr(message)| Api.error(500, "db error: ${message}")
        Ok(rows.map(|row| row.tag))
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: run one article page query plus its unpaginated count. The
## caller supplies the COMPLETE count statement (its FROM clause varies by
## filter); both queries take `where_params` in the same order.
run_article_page! = |ctx, db, viewer, page_where, count_sql, where_params, limit, offset| {
    # The paged summaries as ONE JSON array cell: interpolate the SAME
    # `article_json_summary` the by-slug reader splices (`:viewer` is param 1 —
    # SQLite reuses the one index for the favorited + following flags — so the
    # positional `?` of `page_where` and LIMIT/OFFSET follow it in order), wrap
    # the page in `json_group_array`, and COALESCE an empty page to `[]`.
    array_sql = "SELECT COALESCE(json_group_array(json(summary)), json('[]')) FROM (SELECT ${Db.article_json_summary} AS summary FROM articles a JOIN users u ON u.id = a.author_id${page_where} ORDER BY a.id DESC LIMIT ? OFFSET ?)"
    array_params = [Integer(viewer)]
        .concat(where_params)
        .append(Integer(limit))
        .append(Integer(offset))
    articles = Sql.query_json!(ctx, db, array_sql, array_params)
        ? |message| Api.error(500, "db error: ${message}")

    count_rows = Sql.query!(ctx, db, count_sql, where_params)
        ? |message| Api.error(500, "db error: ${message}")
    count_row = Sql.first(count_rows) ? |_| Api.error(500, "count returned no rows")
    count = count_row.i64("n") ? |_| Api.error(500, "corrupt count row")

    # Envelope assembled in Roc: the big array is spliced once into the body,
    # the count is tiny. Both are already JSON, so `RawJson`, not `Res.json`.
    Ok(RawJson.from("{\"articles\":${RawJson.to_str(articles)},\"articlesCount\":${count.to_str()}}"))
}

## split_tags: NULL means no tags
expect Db.split_tags(NullableStr.Null) == []

## split_tags: GROUP_CONCAT output splits on commas
expect Db.split_tags(NullableStr.Val("a,b")) == ["a", "b"]
