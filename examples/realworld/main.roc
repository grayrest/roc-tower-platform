app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Cofx
import pf.Sql
import Api
import Db
import Auth
import Users
import Profiles
import Articles
import Comments

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
    ],
    init!: init!,
    routes: Users.routes({ prefix: "/api" })
        .concat(Profiles.routes({ prefix: "/api/profiles" }))
        .concat(Articles.routes({ prefix: "/api/articles" }))
        .concat(Comments.routes({ prefix: "/api/articles" }))
        .concat([
            { method: "GET", path: "/api/tags", handler: tags_h! },
        ]),
}

## Coeffect: every tag in use.
tags! = |req| Db.all_tags!(req.ctx, Box.unbox(req.model).db)

## `tags` bound to its coeffect.
tags_h! = Box.box(Cofx.with1(tags!, tags))

## PURE handler: the RealWorld tags envelope.
tags : List(Str) -> Res.Response
tags = |names| Res.json(200, { tags: names })

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

## @schema
ddl_articles =
    \\CREATE TABLE IF NOT EXISTS articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL)

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

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Sql.exec!(0, Db.db_path, sql, []) {
        Ok(_) => Ok({})
        Err(message) => Err("schema: ${message}")
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
    Ok({ db: Db.db_path })
}
