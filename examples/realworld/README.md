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

The read-only GET endpoints (article, list, feed, profile, comments, tags) go a
step further with **`@sql json`**: the JSON document — envelope, camelCase keys,
nested author, tag arrays, boolean flags — is assembled entirely in SQLite
(`json_object`/`json_group_array`/`json_patch`) and returned as raw bytes via
`Res.json_raw`, skipping the record → encode pipeline. The mutate-then-serialize
handlers (`update`, `favorite`, `unfavorite`, `follow`, `unfollow`) keep the
typed reads — they compute the response from the row's fields. See the platform
[README](../../README.md#sql-json--assemble-the-document-in-sql-splice-it-once)
for the annotation reference.

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
