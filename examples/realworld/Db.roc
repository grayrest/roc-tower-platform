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
    db_path : Str
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

    ## A NullableStr as a SQL parameter (Val -> TEXT, Null -> NULL).
    to_sql : NullableStr -> Host.SqlValue
    to_sql = |nullable| match nullable {
        Val(text) => Text(text)
        Null => Null
    }

    ## The article SELECT shared by get/list/feed. The first two params are
    ## the viewer id (favorited flag, following flag); WHERE params follow.
    article_select : Str
    article_select = Str.join_with(
        [
            "SELECT a.id, a.author_id, a.slug, a.title, a.description, a.body, a.created_at, a.updated_at,",
            "u.username, u.bio, u.image,",
            "(SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id) AS favorites_count,",
            "EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = ?) AS favorited,",
            "EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = a.author_id) AS following,",
            "(SELECT GROUP_CONCAT(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)) AS tag_list",
            "FROM articles a JOIN users u ON u.id = a.author_id",
        ],
        " ",
    )

    ## Decode the profile columns present in every article/comment/profile
    ## query: username, bio, image, following.
    decode_profile : Sql.Row -> Try(Db.Profile, Sql.Err)
    decode_profile = |row| {
        username = row.str("username")?
        bio = row.nullable_str("bio")?
        image = row.nullable_str("image")?
        following = row.bool("following")?
        Ok({ username: username, bio: bio, image: image, following: JsonBool.from(following) })
    }

    ## Decode one `article_select` row into the JSON-shaped article.
    decode_article : Sql.Row -> Try(Db.Article, Sql.Err)
    decode_article = |row| {
        slug = row.str("slug")?
        title = row.str("title")?
        description = row.str("description")?
        body = row.str("body")?
        created_at = row.str("created_at")?
        updated_at = row.str("updated_at")?
        favorites_count = row.i64("favorites_count")?
        favorited = row.bool("favorited")?
        tag_list = row.nullable_str("tag_list")?
        author = Db.decode_profile(row)?
        Ok({
            slug: slug,
            title: title,
            description: description,
            body: body,
            tagList: Db.split_tags(tag_list),
            createdAt: created_at,
            updatedAt: updated_at,
            favorited: JsonBool.from(favorited),
            favoritesCount: favorites_count,
            author: author,
        })
    }

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
        id = row.i64("id") ? |_| Api.error(500, "corrupt article row")
        author_id = row.i64("author_id") ? |_| Api.error(500, "corrupt article row")
        article = Db.decode_article(row) ? |_| Api.error(500, "corrupt article row")
        Ok({ id: id, author_id: author_id, article: article })
    }

    ## List articles with optional tag/author/favorited filters, most recent
    ## first, plus the unpaginated count (the `articlesCount` field).
    list_articles! = |db, viewer, filters| {
        tag_part =
            if filters.tag.is_empty()
                { clause: [], params: [] }
            else
                { clause: ["a.id IN (SELECT article_id FROM article_tags WHERE tag = ?)"], params: [Text(filters.tag)] }
        author_part =
            if filters.author.is_empty()
                { clause: [], params: [] }
            else
                { clause: ["u.username = ?"], params: [Text(filters.author)] }
        favorited_part =
            if filters.favorited.is_empty()
                { clause: [], params: [] }
            else
                {
                    clause: ["a.id IN (SELECT fv.article_id FROM favorites fv JOIN users fu ON fu.id = fv.user_id WHERE fu.username = ?)"],
                    params: [Text(filters.favorited)],
                }
        clauses = tag_part.clause.concat(author_part.clause).concat(favorited_part.clause)
        where_params = tag_part.params.concat(author_part.params).concat(favorited_part.params)
        where_sql = if clauses.is_empty() "" else " WHERE ${Str.join_with(clauses, " AND ")}"
        run_article_page!(db, viewer, where_sql, where_params, filters.limit, filters.offset)
    }

    ## The viewer's feed: articles by followed authors, most recent first.
    feed_articles! = |db, viewer, limit, offset| {
        where_sql = " WHERE a.author_id IN (SELECT followed_id FROM follows WHERE follower_id = ?)"
        run_article_page!(db, viewer, where_sql, [Integer(viewer)], limit, offset)
    }

    ## Load a profile by username with the viewer's following flag; rejects
    ## 404 on a miss. Returns the user id alongside for follow writes.
    load_profile! = |db, viewer, username| {
        sql = Str.join_with(
            [
                "SELECT u.id, u.username, u.bio, u.image,",
                "EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = u.id) AS following",
                "FROM users u WHERE u.username = ?",
            ],
            " ",
        )
        result = Host.sqlite_exec!({ db: db, sql: sql, params: [Integer(viewer), Text(username)] })
            ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.not_found("profile")
        id = row.i64("id") ? |_| Api.error(500, "corrupt user row")
        profile = Db.decode_profile(row) ? |_| Api.error(500, "corrupt user row")
        Ok({ id: id, profile: profile })
    }

    ## All comments on an article, oldest first, with viewer following flags.
    load_comments! = |db, viewer, article_id| {
        sql = Str.join_with(
            [
                "SELECT c.id, c.body, c.created_at, c.updated_at, u.username, u.bio, u.image,",
                "EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = c.author_id) AS following",
                "FROM comments c JOIN users u ON u.id = c.author_id WHERE c.article_id = ? ORDER BY c.id",
            ],
            " ",
        )
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
    decode_comment = |row| {
        id = row.i64("id")?
        body = row.str("body")?
        created_at = row.str("created_at")?
        updated_at = row.str("updated_at")?
        author = Db.decode_profile(row)?
        Ok({ id: id, createdAt: created_at, updatedAt: updated_at, body: body, author: author })
    }

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

## Internal: run one article page query plus its unpaginated count.
run_article_page! = |db, viewer, where_sql, where_params, limit, offset| {
    list_sql = "${Db.article_select}${where_sql} ORDER BY a.id DESC LIMIT ? OFFSET ?"
    list_params = [Integer(viewer), Integer(viewer)]
        .concat(where_params)
        .append(Integer(limit))
        .append(Integer(offset))
    list_result = Host.sqlite_exec!({ db: db, sql: list_sql, params: list_params })
        ? |message| Api.error(500, "db error: ${message}")
    articles = Db.decode_articles(list_result) ? |_| Api.error(500, "corrupt article row")

    count_sql = "SELECT COUNT(*) AS n FROM articles a JOIN users u ON u.id = a.author_id${where_sql}"
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
        columns: ["id", "slug", "title", "description", "body", "created_at", "updated_at", "username", "bio", "image", "favorites_count", "favorited", "following", "tag_list"],
        cells: [Integer(7), Text("how-to-7"), Text("How To"), Text("d"), Text("b"), Text("2026-07-12T00:00:00.000Z"), Text("2026-07-12T00:00:00.000Z"), Text("jake"), Null, Null, Integer(2), Integer(0), Integer(1), Text("dragons,training")],
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
