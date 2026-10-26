app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Sql
import Api
import Db
import Auth
import Users
import Profiles
import Articles
import Comments
import Tags

# RealWorld (Conduit) backend — https://realworld-docs.netlify.app — as a
# stress test of the coeffect API: every handler is pure, every read is a
# coeffect, every write is a host-run effect. Routes live with their
# resource modules; this file owns the model, settings, and schema.
Model : { db : Str }

main = |{}| {
    settings: [
        CorsPermissive,
        TimeoutMs(10000),
        MaxBodyBytes(65536),
        # In-memory response cache for anonymous GETs. Invalidation is precise —
        # a write purges exactly the entries whose derived reads it touched — so
        # the TTL is only a backstop for anything the platform can't see (an
        # out-of-band DB write). `bypass_headers` is left empty: the platform
        # hard-defaults `authorization`, so credentialed requests are excluded
        # from the shared cache (both served-from and stored-into) without the
        # app having to remember to list it.
        ResponseCache({ ttl_ms: 30000, max_entries: 4096, methods: ["GET"], bypass_headers: [] }),
    ],
    init!: init!,
    # Each module applies the shared `Api.route` (and its baked-in `Api.render`)
    # directly, so no `route` field is threaded in.
    routes: Users.routes({ prefix: "/api" })
        .concat(Profiles.routes({ prefix: "/api/profiles" }))
        .concat(Articles.routes({ prefix: "/api/articles" }))
        .concat(Comments.routes({ prefix: "/api/articles" }))
        .concat(Tags.routes({ prefix: "/api/tags" })),
}

# --- Schema ------------------------------------------------------------------
#
# `## @schema`-marked constants are the SINGLE source of truth: sqlgen applies
# them to an in-memory database to type the `## @sql` queries, and init! applies
# the same constants at startup. `IF NOT EXISTS` is harmless against sqlgen's
# fresh :memory:. Declaration order is apply order.

## @schema
ddl_users =
    \\CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT)

## @schema
ddl_tokens =
    \\CREATE TABLE IF NOT EXISTS tokens (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id))

## @schema
ddl_follows =
    \\CREATE TABLE IF NOT EXISTS follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id))

## The two viewer-INDEPENDENT aggregates (`favorites_count`, `tag_list`) are
## MATERIALIZED onto the row — identical for every reader, so computing them
## per-request-per-row was the dominant DB cost of a list page. Trigger-
## maintained (see `ddl_trg_*`); reads splice `a.favorites_count` / `a.tag_list`
## as plain columns instead of correlated subqueries. `tag_list` is the sorted
## GROUP_CONCAT (NULL when the article has no tags).
## @schema
ddl_articles =
    \\CREATE TABLE IF NOT EXISTS articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, favorites_count INTEGER NOT NULL DEFAULT 0, tag_list TEXT)

## @schema
ddl_article_tags =
    \\CREATE TABLE IF NOT EXISTS article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag))

## @schema
ddl_favorites =
    \\CREATE TABLE IF NOT EXISTS favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id))

## @schema
ddl_comments =
    \\CREATE TABLE IF NOT EXISTS comments (id INTEGER PRIMARY KEY, article_id INTEGER NOT NULL REFERENCES articles(id), author_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)

# Indexes for every WHERE/JOIN the queries actually run. Without these, each
# article list row pays TWO full favorites scans (count + viewer EXISTS), and
# tag/author filters and comment lists scan their tables.
## @schema
ddl_idx_articles_author =
    \\CREATE INDEX IF NOT EXISTS idx_articles_author ON articles(author_id)

## @schema
ddl_idx_article_tags_tag =
    \\CREATE INDEX IF NOT EXISTS idx_article_tags_tag ON article_tags(tag)

## @schema
ddl_idx_favorites_article =
    \\CREATE INDEX IF NOT EXISTS idx_favorites_article ON favorites(article_id, user_id)

## @schema
ddl_idx_comments_article =
    \\CREATE INDEX IF NOT EXISTS idx_comments_article ON comments(article_id)

# Triggers keep the materialized `articles.favorites_count` / `articles.tag_list`
# in sync with the join tables, so every article READ is a plain column fetch
# instead of a correlated subquery. Writes are rare, reads are hot — this trades
# a little write cost for a ~30-40% cheaper list page. `INSERT OR IGNORE` that
# hits the PK is a no-op and does NOT fire AFTER INSERT (count stays correct on
# a repeat favorite); the same holds for a DELETE that matches nothing.
## @schema
ddl_trg_fav_insert =
    \\CREATE TRIGGER IF NOT EXISTS trg_fav_insert AFTER INSERT ON favorites
    \\BEGIN
    \\  UPDATE articles SET favorites_count = favorites_count + 1 WHERE id = NEW.article_id;
    \\END

## @schema
ddl_trg_fav_delete =
    \\CREATE TRIGGER IF NOT EXISTS trg_fav_delete AFTER DELETE ON favorites
    \\BEGIN
    \\  UPDATE articles SET favorites_count = favorites_count - 1 WHERE id = OLD.article_id;
    \\END

## @schema
ddl_trg_tag_insert =
    \\CREATE TRIGGER IF NOT EXISTS trg_tag_insert AFTER INSERT ON article_tags
    \\BEGIN
    \\  UPDATE articles SET tag_list = (SELECT GROUP_CONCAT(tag) FROM (SELECT tag FROM article_tags WHERE article_id = NEW.article_id ORDER BY tag)) WHERE id = NEW.article_id;
    \\END

## @schema
ddl_trg_tag_delete =
    \\CREATE TRIGGER IF NOT EXISTS trg_tag_delete AFTER DELETE ON article_tags
    \\BEGIN
    \\  UPDATE articles SET tag_list = (SELECT GROUP_CONCAT(tag) FROM (SELECT tag FROM article_tags WHERE article_id = OLD.article_id ORDER BY tag)) WHERE id = OLD.article_id;
    \\END

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Sql.migrate!(Db.db_path, sql, []) {
        Ok(_) => Ok({})
        Err(DbErr(message)) => Err("schema: ${message}")
    }
    exec!(ddl_users)?
    exec!(ddl_tokens)?
    exec!(ddl_follows)?
    exec!(ddl_articles)?
    exec!(ddl_article_tags)?
    exec!(ddl_favorites)?
    exec!(ddl_comments)?
    exec!(ddl_idx_articles_author)?
    exec!(ddl_idx_article_tags_tag)?
    exec!(ddl_idx_favorites_article)?
    exec!(ddl_idx_comments_article)?
    exec!(ddl_trg_fav_insert)?
    exec!(ddl_trg_fav_delete)?
    exec!(ddl_trg_tag_insert)?
    exec!(ddl_trg_tag_delete)?
    Ok({ db: Db.db_path })
}
