import pf.Host
import pf.Sql
import pf.NullableStr
import pf.JsonBool
import pf.JsonEnc
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
        \\       a.favorites_count,
        \\       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer) AS favorited,
        \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id) AS following,
        \\       a.tag_list
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
            NoRows => Fail(404.U16, "article", "not found")
            DbErr(message) => DbErr(message)
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

    ## The LIGHT article lookup for existence and ownership checks: just the
    ## row id and author, no joins/aggregates. Use `load_article!` only when
    ## the response needs the full JSON-shaped article.
    ## @params slug: Str
    ## @sql one
    article_meta_q =
        \\SELECT id, author_id FROM articles WHERE slug = :slug

    load_article_meta! = |ctx, db, slug| {
        row = DbGen.article_meta_q!(ctx, db, { slug: slug }) ? |err| match err {
            NoRows => Fail(404.U16, "article", "not found")
            DbErr(message) => DbErr(message)
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
    ## PURE: turn the list filters into the shared page WHERE, the count
    ## statement, and the ordered positional `where_params` — the exact tuple
    ## `run_article_page!` takes.
    list_query_parts = |filters| {
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
        # The COUNT never selects user columns, so it only joins `users` when a
        # filter actually constrains one (the author filter's `u.username = ?`).
        # SQLite can't prove an inner join to a NOT-NULL FK target is a count
        # no-op, so an unnecessary join costs a PK probe PER ROW — ~25x on the
        # 5000-row unfiltered count (147us -> 6us). The page query still joins
        # (it selects username/bio/image); only the count is trimmed.
        needs_user_join = !filters.author.is_empty()
        count_sql =
            if tag_only
                "SELECT COUNT(*) AS n FROM article_tags att${count_where}"
            else if filters.tag.is_empty() and !needs_user_join
                "SELECT COUNT(*) AS n FROM articles a${count_where}"
            else if filters.tag.is_empty()
                "SELECT COUNT(*) AS n FROM articles a JOIN users u ON u.id = a.author_id${count_where}"
            else if needs_user_join
                "SELECT COUNT(*) AS n FROM article_tags att JOIN articles a ON a.id = att.article_id JOIN users u ON u.id = a.author_id${count_where}"
            else
                "SELECT COUNT(*) AS n FROM article_tags att JOIN articles a ON a.id = att.article_id${count_where}"
        { page_where: page_where, count_sql: count_sql, where_params: where_params }
    }

    ## List: same filter parts, typed rows serialized in Roc.
    ## STREAMING list encoder: build the `{articles:[...],articlesCount:N}` bytes
    ## straight off the cursor, never materializing the 20 ArticleSummary records
    ## or their ~200 string fields. Each cell is read, written to the buffer, and
    ## dropped in place. Returns the response body bytes for `Res.bytes`.
    stream_list! : U64, Str, I64, { tag : Str, author : Str, favorited : Str, limit : I64, offset : I64 } => Try(List(U8), [Fail(U16, Str, Str), ..])
    stream_list! = |ctx, db, viewer, filters| {
        parts = Db.list_query_parts(filters)
        is_anon = viewer == 0
        select = if is_anon article_summary_select_anon else article_summary_select
        page_sql = "${select}${parts.page_where} ORDER BY a.id DESC LIMIT ? OFFSET ?"
        viewer_params = if is_anon [] else [Integer(viewer)]
        page_params = viewer_params.concat(parts.where_params).append(Integer(filters.limit)).append(Integer(filters.offset))
        stream_page!(ctx, db, page_sql, page_params, parts.count_sql, parts.where_params)
    }

    ## STREAMING feed encoder — same, over the feed page SQL.
    stream_feed! : U64, Str, I64, I64, I64 => Try(List(U8), [Fail(U16, Str, Str), ..])
    stream_feed! = |ctx, db, viewer, limit, offset| {
        page_sql = "${article_summary_select}${Db.feed_where} ORDER BY a.id DESC LIMIT ? OFFSET ?"
        page_params = [Integer(viewer), Integer(viewer), Integer(limit), Integer(offset)]
        stream_page!(ctx, db, page_sql, page_params, Db.feed_count, [Integer(viewer)])
    }

    ## The viewer's feed: articles by followed authors, most recent first.
    feed_where = " WHERE EXISTS(SELECT 1 FROM follows fo2 WHERE fo2.follower_id = ? AND fo2.followed_id = a.author_id)"
    feed_count = "SELECT COUNT(*) AS n FROM articles a WHERE a.author_id IN (SELECT followed_id FROM follows WHERE follower_id = ?)"

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
            NoRows => Fail(404.U16, "profile", "not found")
            DbErr(message) => DbErr(message)
        }
        Ok({
            id: row.id,
            profile: { username: row.username, bio: row.bio, image: row.image, following: row.following },
        })
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

    ## Named-binding write builders (@sql resolves :name -> positional at
    ## codegen), so a param order can never drift from the SQL. The register
    ## flow uses these instead of hand-built positional DbWrite payloads.
    ## @params id: I64, email: Str, username: Str, password_hash: Str
    ## @sql
    insert_user =
        \\INSERT INTO users (id, email, username, password_hash) VALUES (:id, :email, :username, :password_hash)

    ## @params token: Str, user_id: I64
    ## @sql
    insert_token =
        \\INSERT INTO tokens (token, user_id) VALUES (:token, :user_id)

    load_comments! = |ctx, db, viewer, article_id| {
        rows = DbGen.comments_q!(ctx, db, { viewer: viewer, article_id: article_id })?
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
            NoRows => Fail(404.U16, "comment", "not found")
            DbErr(message) => DbErr(message)
        }
        Ok(row)
    }

    ## GET /tags: every tag in use, sorted. The Roc side pulls the one column
    ## out to a `List(Str)` and shapes the `{tags}` envelope.
    ## @sql
    tags_q =
        \\SELECT DISTINCT tag FROM article_tags ORDER BY tag

    all_tags! = |ctx, db| {
        rows = DbGen.tags_q!(ctx, db, {})?
        Ok(rows.map(|row| row.tag))
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: the body-less article columns for list/feed rows — mirrors
## `article_select` (the by-slug typed fragment) but OMITS `a.body`, which the
## spec forbids in list items. `favorites_count`/`tag_list` are the MATERIALIZED
## columns (trigger-maintained), so the only per-row work left is the two viewer
## flags. Read by name via `Sql.query!`; `:viewer` is param 1 (SQLite reuses it
## for both flags), page_where `?` follow it.
article_summary_select =
    \\SELECT a.slug, a.title, a.description, a.created_at, a.updated_at,
    \\       u.username, u.bio, u.image,
    \\       a.favorites_count,
    \\       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer) AS favorited,
    \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id) AS following,
    \\       a.tag_list
    \\FROM articles a JOIN users u ON u.id = a.author_id

## Internal: the ANONYMOUS (viewer = 0) list variant. An unauthenticated viewer
## favorites nothing and follows nobody, so both flags are constant 0 and the two
## EXISTS subqueries are pure waste — dropped here. No `:viewer` parameter, so the
## page params carry no leading viewer id. Same column set/names as
## `article_summary_select` (the by-name row mapper is shared).
article_summary_select_anon =
    \\SELECT a.slug, a.title, a.description, a.created_at, a.updated_at,
    \\       u.username, u.bio, u.image,
    \\       a.favorites_count,
    \\       0 AS favorited,
    \\       0 AS following,
    \\       a.tag_list
    \\FROM articles a JOIN users u ON u.id = a.author_id


## Internal: read the page as typed rows (body-less), read the count, and hand
## the `{articles, articlesCount}` record back for the pure handler to
## serialize in Roc. `where_params` bind the page_where `?` in order.

## split_tags: NULL means no tags

# --- streaming JSON encoder (module-private, effectful) --------------------
#
# Fuse the cursor read with the JSON write: append each cell straight to the
# byte buffer, so the article records are never built. Field order matches the
# derived encoder's alphabetical output so responses are byte-identical.

## Open the page cursor, stream every row into the articles array, then read
## the count and close the envelope.
stream_page! : U64, Str, Str, List(Host.SqlValue), Str, List(Host.SqlValue) => Try(List(U8), [Fail(U16, Str, Str), ..])
stream_page! = |ctx, db, page_sql, page_params, count_sql, count_params| {
    handle = Host.sql_begin!({ ctx: ctx, db: db, sql: page_sql, params: page_params }) ? |m| Fail(500.U16, "body", m)
    # On a mid-stream error the request tears down and the host sweeps this
    # unfinished cursor (Host.sql_begin! contract), so a bare `?` is safe.
    body = stream_rows!(handle, JsonEnc.raw_into([], "{\"articles\":["), True)?
    Host.sql_finish!(handle)
    chandle = Host.sql_begin!({ ctx: ctx, db: db, sql: count_sql, params: count_params }) ? |m| Fail(500.U16, "body", m)
    count = read_count!(chandle)
    Host.sql_finish!(chandle)
    Ok(JsonEnc.raw_into(JsonEnc.i64_into(JsonEnc.raw_into(body, "],\"articlesCount\":"), count), "}"))
}

## Step the cursor, append each article object (comma-separated), recurse.
stream_rows! : U64, List(U8), Bool => Try(List(U8), [Fail(U16, Str, Str), ..])
stream_rows! = |handle, out, first| {
    # `sql_step_row!` advances the cursor and returns a fresh generation TOKEN
    # for the new row (0 = Done). Threading that token into the PURE
    # `sql_row_str` reads is what makes them sound: the token pins the row, so
    # a read can never silently observe a later step's data (H1).
    match Host.sql_step_row!(handle) {
        Err(m) => Err(Fail(500.U16, "body", m))
        Ok(tok) =>
            if tok != 0 {
                separated = if first out else JsonEnc.raw_into(out, ",")
                stream_rows!(handle, stream_article!(handle, tok, separated), False)
            } else {
                Ok(out)
            }
    }
}

## One article object, read cell-by-cell (column order: slug 0, title 1,
## description 2, created_at 3, updated_at 4, username 5, bio 6, image 7,
## favorites_count 8, favorited 9, following 10, tag_list 11). JSON keys are
## emitted alphabetically to match the derived encoder. String cells are read
## through the PURE `Host.sql_row_str(handle, tok, idx)`; int/type reads still
## use the effectful getters (col_str-first scope).
stream_article! : U64, U64, List(U8) => List(U8)
stream_article! = |h, tok, out|
    out
    |> JsonEnc.raw_into("{\"author\":{\"bio\":")
    |> Host.sql_col_nullable_str_json_into(h, tok, 6)
    |> JsonEnc.raw_into(",\"following\":")
    |> JsonEnc.bool_into(Host.sql_col_i64!(h, 10) != 0)
    |> JsonEnc.raw_into(",\"image\":")
    |> Host.sql_col_nullable_str_json_into(h, tok, 7)
    |> JsonEnc.raw_into(",\"username\":")
    |> Host.sql_col_str_json_into(h, tok, 5)
    |> JsonEnc.raw_into("},\"createdAt\":")
    |> Host.sql_col_str_json_into(h, tok, 3)
    |> JsonEnc.raw_into(",\"description\":")
    |> Host.sql_col_str_json_into(h, tok, 2)
    |> JsonEnc.raw_into(",\"favorited\":")
    |> JsonEnc.bool_into(Host.sql_col_i64!(h, 9) != 0)
    |> JsonEnc.raw_into(",\"favoritesCount\":")
    |> JsonEnc.i64_into(Host.sql_col_i64!(h, 8))
    |> JsonEnc.raw_into(",\"slug\":")
    |> Host.sql_col_str_json_into(h, tok, 0)
    |> JsonEnc.raw_into(",\"tagList\":")
    |> Host.sql_col_taglist_json_into(h, tok, 11)
    |> JsonEnc.raw_into(",\"title\":")
    |> Host.sql_col_str_json_into(h, tok, 1)
    |> JsonEnc.raw_into(",\"updatedAt\":")
    |> Host.sql_col_str_json_into(h, tok, 4)
    |> JsonEnc.raw_into("}")
## Read the single COUNT(*) cell.
read_count! : U64 => I64
read_count! = |handle|
    match Host.sql_step!(handle) {
        Ok(has_row) => if has_row Host.sql_col_i64!(handle, 0) else 0
        Err(_) => 0
    }

expect Db.split_tags(NullableStr.Null) == []

## split_tags: GROUP_CONCAT output splits on commas
expect Db.split_tags(NullableStr.Val("a,b")) == ["a", "b"]
