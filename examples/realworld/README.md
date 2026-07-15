# realworld

A complete [RealWorld/Conduit](https://realworld-docs.netlify.app) backend and
the platform's reference app — 19 endpoints (auth with opaque tokens and
argon2 hashing, profiles/follows, article CRUD, feed, favorites, comments,
tags) with every handler pure: reads are composed coeffects (auth is one
memoized lookup shared by `current_user!`/`optional_user!`), validation
rejections ARE the error responses, and all writes are host-run transactional
effects. It passes the official RealWorld conformance suite, 154 requests:
`just realworld-conformance` (needs `hurl`).

## Typed SQL

The fixed-shape reads are **generated** by `sqlgen` from inline `## @sql`
queries typed against the `## @schema` DDL in [main.roc](main.roc) — see
[Db.roc](Db.roc) for the queries and the committed [DbGen.roc](DbGen.roc) for
the emitted typed readers (`just sqlgen` regenerates, `just sqlgen-check`
gates drift). The shared article projection lives once as a `## @fragment`;
the by-slug read splices it at codegen time while the dynamic article
list/feed interpolate the same constant at request time.

Every endpoint — reads and mutate-then-serialize alike — reads typed rows and
serializes its response envelope in Roc (`Res.json(status, record)`): flat rows
mapped to the JSON shape (nesting the author, splitting tag lists, flags to
`JsonBool`). An earlier `@sql json` variant assembled the document in SQLite and
passed it through verbatim; it was removed after a both-engine A/B
([../../notes/2026-07-15-typed-vs-json.md](../../notes/2026-07-15-typed-vs-json.md))
in favor of the single typed surface. The dynamic list/feed still interpolate
the shared projection at request time via `Sql.query!` (by-name reads).

## Running

```sh
just run examples/realworld/main.roc

# with a custom Roc checkout
ROC=/path/to/roc just run examples/realworld/main.roc
```

Binds the `PORT` env var (default 8000); data lives in
`/tmp/tower-platform-realworld.db`:

```sh
curl -X POST localhost:8000/api/users \
    -d '{"user":{"username":"jake","email":"jake@x.com","password":"dragons4ever"}}'
curl localhost:8000/api/articles
```
