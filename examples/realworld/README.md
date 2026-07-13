# realworld

A complete [RealWorld/Conduit](https://realworld-docs.netlify.app) backend and
the platform's reference app — 19 endpoints (auth with opaque tokens and
argon2 hashing, profiles/follows, article CRUD, feed, favorites, comments,
tags) with every handler pure: reads are composed coeffects (auth is one
memoized lookup shared by `current_user!`/`optional_user!`), validation
rejections ARE the error responses, and all writes are host-run transactional
effects. It passes the official RealWorld conformance suite, 154 requests:
`just realworld-conformance` (needs `hurl`).

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
