app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Cofx
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
    routes: Users.routes({})
        .concat(Profiles.routes({}))
        .concat(Articles.routes({}))
        .concat(Comments.routes({}))
        .concat([
            { method: "GET", path: "/api/tags", handler: Box.box(Cofx.with1(tags!, tags)) },
        ]),
}

## Coeffect: every tag in use.
tags! = |req| Db.all_tags!(Box.unbox(req.model).db)

## PURE handler: the RealWorld tags envelope.
tags : List(Str) -> Res.Response
tags = |names| Res.json(200, { tags: names })

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Host.sqlite_exec!({ db: Db.db_path, sql: sql, params: [] }) {
        Ok(_) => Ok({})
        Err(message) => Err("schema: ${message}")
    }
    exec!("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT)")?
    exec!("CREATE TABLE IF NOT EXISTS tokens (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id))")?
    exec!("CREATE TABLE IF NOT EXISTS follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id))")?
    exec!("CREATE TABLE IF NOT EXISTS articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL)")?
    exec!("CREATE TABLE IF NOT EXISTS article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag))")?
    exec!("CREATE TABLE IF NOT EXISTS favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id))")?
    exec!("CREATE TABLE IF NOT EXISTS comments (id INTEGER PRIMARY KEY, article_id INTEGER NOT NULL REFERENCES articles(id), author_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)")?
    Ok({ db: Db.db_path })
}
