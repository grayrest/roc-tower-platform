import pf.Host
import pf.Sql
import pf.NullableStr
import pf.JsonBool
import Api

## Schema, queries, and row decoders for the RealWorld data model. Reads run
## inside coeffect resolvers (`Host.sqlite_exec!`); writes are built by pure
## handlers as `DbWrite` effects against the same `db_path`.
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

    ## RealWorld comment object, JSON-shaped.
    Comment : { id : I64, createdAt : Str, updatedAt : Str, body : Str, author : Db.Profile }

    ## An article as list/feed endpoints show it — the spec REQUIRES `body`
    ## to be absent from multi-article responses.
    Summary : {
        slug : Str,
        title : Str,
        description : Str,
        tagList : List(Str),
        createdAt : Str,
        updatedAt : Str,
        favorited : JsonBool,
        favoritesCount : I64,
        author : Db.Profile,
    }

    ## Drop the body for a list response.
    to_summary : Db.Article -> Db.Summary
    to_summary = |article| {
        slug: article.slug,
        title: article.title,
        description: article.description,
        tagList: article.tagList,
        createdAt: article.createdAt,
        updatedAt: article.updatedAt,
        favorited: article.favorited,
        favoritesCount: article.favoritesCount,
        author: article.author,
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

    ## The article SELECT shared by get/list/feed. The first two params are
    ## the viewer id (favorited flag, following flag); WHERE params follow.
    article_select =
        \\SELECT a.id, a.author_id, a.slug, a.title, a.description, a.body, a.created_at, a.updated_at,
        \\       u.username, u.bio, u.image,
        \\       (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id) AS favorites_count,
        \\       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = ?) AS favorited,
        \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = a.author_id) AS following,
        \\       (SELECT GROUP_CONCAT(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)) AS tag_list
        \\FROM articles a JOIN users u ON u.id = a.author_id

    ## Decode the profile columns present in every article/comment/profile
    ## query: username, bio, image, following.
    decode_profile : Sql.Row -> Try(Db.Profile, Sql.Err)
    decode_profile = |row| Ok({
        username: row.str("username")?,
        bio: row.nullable_str("bio")?,
        image: row.nullable_str("image")?,
        following: JsonBool.from(row.bool("following")?),
    })

    ## Decode one `article_select` row into the JSON-shaped article.
    decode_article : Sql.Row -> Try(Db.Article, Sql.Err)
    decode_article = |row| Ok({
        slug: row.str("slug")?,
        title: row.str("title")?,
        description: row.str("description")?,
        body: row.str("body")?,
        tagList: Db.split_tags(row.nullable_str("tag_list")?),
        createdAt: row.str("created_at")?,
        updatedAt: row.str("updated_at")?,
        favorited: JsonBool.from(row.bool("favorited")?),
        favoritesCount: row.i64("favorites_count")?,
        author: Db.decode_profile(row)?,
    })

    ## GROUP_CONCAT output to a tag list: NULL (no tags) and "" become [].
    split_tags : NullableStr -> List(Str)
    split_tags = |joined| match joined {
        Null => []
        Val(text) => if text.is_empty() [] else text.split_on(",")
    }

    ## Decode every row of an article query, first failure wins.
    decode_articles : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> Try(List(Db.Article), Sql.Err)
    decode_articles = |result| {
        Sql.rows(result).fold(Ok([]), |acc, row| {
            match acc {
                Err(problem) => Err(problem)
                Ok(articles) => match Db.decode_article(row) {
                    Ok(article) => Ok(articles.append(article))
                    Err(problem) => Err(problem)
                }
            }
        })
    }

    ## Load one article by slug with the viewer's flags; rejects 404 on a
    ## miss. Returns the row id alongside the JSON-shaped article for
    ## author checks, updates, and join-table writes.
    load_article! = |db, viewer, slug| {
        sql = "${Db.article_select} WHERE a.slug = ?"
        result = Host.sqlite_exec!({ db: db, sql: sql, params: [Integer(viewer), Integer(viewer), Text(slug)] })
            ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.not_found("article")
        Ok({
            id: row.i64("id") ? |_| Api.error(500, "corrupt article row"),
            author_id: row.i64("author_id") ? |_| Api.error(500, "corrupt article row"),
            article: Db.decode_article(row) ? |_| Api.error(500, "corrupt article row"),
        })
    }

    ## The LIGHT article lookup for existence and ownership checks: just the
    ## row id and author, no joins/aggregates. Use `load_article!` only when
    ## the response needs the full JSON-shaped article.
    load_article_meta! = |db, slug| {
        result = Host.sqlite_exec!({ db: db, sql: "SELECT id, author_id FROM articles WHERE slug = ?", params: [Text(slug)] })
            ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.not_found("article")
        Ok({
            id: row.i64("id") ? |_| Api.error(500, "corrupt article row"),
            author_id: row.i64("author_id") ? |_| Api.error(500, "corrupt article row"),
        })
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
    list_articles! = |db, viewer, filters| {
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
        run_article_page!(db, viewer, page_where, count_sql, where_params, filters.limit, filters.offset)
    }

    ## The viewer's feed: articles by followed authors, most recent first.
    ## Same EXISTS-for-page / IN-for-count split as `list_articles!`.
    feed_articles! = |db, viewer, limit, offset| {
        page_where = " WHERE EXISTS(SELECT 1 FROM follows fo2 WHERE fo2.follower_id = ? AND fo2.followed_id = a.author_id)"
        count_sql = "SELECT COUNT(*) AS n FROM articles a WHERE a.author_id IN (SELECT followed_id FROM follows WHERE follower_id = ?)"
        run_article_page!(db, viewer, page_where, count_sql, [Integer(viewer)], limit, offset)
    }

    ## Load a profile by username with the viewer's following flag; rejects
    ## 404 on a miss. Returns the user id alongside for follow writes.
    load_profile! = |db, viewer, username| {
        sql =
            \\SELECT u.id, u.username, u.bio, u.image,
            \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = u.id) AS following
            \\FROM users u WHERE u.username = ?
        result = Host.sqlite_exec!({ db: db, sql: sql, params: [Integer(viewer), Text(username)] })
            ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.not_found("profile")
        Ok({
            id: row.i64("id") ? |_| Api.error(500, "corrupt user row"),
            profile: Db.decode_profile(row) ? |_| Api.error(500, "corrupt user row"),
        })
    }

    ## All comments on an article, oldest first, with viewer following flags.
    load_comments! = |db, viewer, article_id| {
        sql =
            \\SELECT c.id, c.body, c.created_at, c.updated_at, u.username, u.bio, u.image,
            \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = c.author_id) AS following
            \\FROM comments c JOIN users u ON u.id = c.author_id WHERE c.article_id = ? ORDER BY c.id
        result = Host.sqlite_exec!({ db: db, sql: sql, params: [Integer(viewer), Integer(article_id)] })
            ? |message| Api.error(500, "db error: ${message}")
        Sql.rows(result).fold(Ok([]), |acc, row| {
            match acc {
                Err(response) => Err(response)
                Ok(comments) => match Db.decode_comment(row) {
                    Ok(comment) => Ok(comments.append(comment))
                    Err(_) => Err(Api.error(500, "corrupt comment row"))
                }
            }
        })
    }

    ## Decode one comment row (columns per `load_comments!`).
    decode_comment : Sql.Row -> Try(Db.Comment, Sql.Err)
    decode_comment = |row| Ok({
        id: row.i64("id")?,
        createdAt: row.str("created_at")?,
        updatedAt: row.str("updated_at")?,
        body: row.str("body")?,
        author: Db.decode_profile(row)?,
    })

    ## One comment's id + author for the delete-comment authorization check;
    ## scoped to the article so a wrong-article id 404s.
    load_comment! = |db, comment_id, article_id| {
        sql = "SELECT c.id, c.author_id FROM comments c WHERE c.id = ? AND c.article_id = ?"
        result = Host.sqlite_exec!({ db: db, sql: sql, params: [Integer(comment_id), Integer(article_id)] })
            ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.not_found("comment")
        id = row.i64("id") ? |_| Api.error(500, "corrupt comment row")
        author_id = row.i64("author_id") ? |_| Api.error(500, "corrupt comment row")
        Ok({ id: id, author_id: author_id })
    }

    ## Every tag in use, sorted.
    all_tags! = |db| {
        result = Host.sqlite_exec!({ db: db, sql: "SELECT DISTINCT tag FROM article_tags ORDER BY tag", params: [] })
            ? |message| Api.error(500, "db error: ${message}")
        Sql.rows(result).fold(Ok([]), |acc, row| {
            match acc {
                Err(response) => Err(response)
                Ok(tags) => match row.str("tag") {
                    Ok(tag) => Ok(tags.append(tag))
                    Err(_) => Err(Api.error(500, "corrupt tag row"))
                }
            }
        })
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: run one article page query plus its unpaginated count. The
## caller supplies the COMPLETE count statement (its FROM clause varies by
## filter); both queries take `where_params` in the same order.
run_article_page! = |db, viewer, page_where, count_sql, where_params, limit, offset| {
    list_sql = "${Db.article_select}${page_where} ORDER BY a.id DESC LIMIT ? OFFSET ?"
    list_params = [Integer(viewer), Integer(viewer)]
        .concat(where_params)
        .append(Integer(limit))
        .append(Integer(offset))
    list_result = Host.sqlite_exec!({ db: db, sql: list_sql, params: list_params })
        ? |message| Api.error(500, "db error: ${message}")
    articles = Db.decode_articles(list_result) ? |_| Api.error(500, "corrupt article row")

    count_result = Host.sqlite_exec!({ db: db, sql: count_sql, params: where_params })
        ? |message| Api.error(500, "db error: ${message}")
    count_row = Sql.first(count_result) ? |_| Api.error(500, "count returned no rows")
    count = count_row.i64("n") ? |_| Api.error(500, "corrupt count row")
    Ok({ articles: articles, articlesCount: count })
}

## split_tags: NULL means no tags
expect Db.split_tags(NullableStr.Null) == []

## split_tags: GROUP_CONCAT output splits on commas
expect Db.split_tags(NullableStr.Val("a,b")) == ["a", "b"]

## decode_profile reads the shared profile columns incl. the following flag
expect {
    row = Sql.Row.{ columns: ["username", "bio", "image", "following"], cells: [Text("jake"), Null, Text("http://x/p.png"), Integer(1)] }
    Db.decode_profile(row)
    == Ok({ username: "jake", bio: NullableStr.Null, image: NullableStr.Val("http://x/p.png"), following: JsonBool.from(True) })
}

## decode_article turns one joined row into the JSON-shaped article object
expect {
    row = Sql.Row.{
        columns: ["id", "author_id", "slug", "title", "description", "body", "created_at", "updated_at", "username", "bio", "image", "favorites_count", "favorited", "following", "tag_list"],
        cells: [Integer(7), Integer(1), Text("how-to-7"), Text("How To"), Text("d"), Text("b"), Text("2026-07-12T00:00:00.000Z"), Text("2026-07-12T00:00:00.000Z"), Text("jake"), Null, Null, Integer(2), Integer(0), Integer(1), Text("dragons,training")],
    }
    match Db.decode_article(row) {
        Ok(article) => article.slug == "how-to-7" and article.tagList == ["dragons", "training"] and article.favoritesCount == 2
        Err(_) => False
    }
}

## a profile row missing an expected column fails loudly, not silently
expect {
    row = Sql.Row.{ columns: ["username"], cells: [Text("jake")] }
    Db.decode_profile(row) == Err(NoColumn("bio"))
}
