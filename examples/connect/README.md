# Connect RPC demo (unary, JSON)

A [Connect protocol](https://connectrpc.com/docs/protocol/) service on the
tower-platform — which exists mostly to test that read-only RPCs are cached
automatically on their request body.

Only the unary + JSON subset is implemented. That subset is plain HTTP/1.1:
no length-prefix framing, no HTTP trailers, no HTTP/2, no streaming. Those only
appear in the streaming/binary variants, which are out of scope here.

## The service

`tower.connect.v1.CatalogService` (schema in [`catalog.proto`](catalog.proto)):

| Method | Kind | Cache behavior |
|---|---|---|
| `GetBook({id}) -> {book}` | read | cached, keyed on the request body (per id) |
| `ListBooks({}) -> {books}` | read | cached; no body read, so one shared entry |
| `AddBook({title,author,year}) -> {book}` | write | `DbWrite` → purges cached reads of `books` |
| `DeleteBook({id}) -> {}` | write | `DbWrite` → purges cached reads of `books` |

## Run it

```sh
just build examples/connect/main.roc     # produces ./main
PORT=8000 ./main
```

```sh
SVC=http://127.0.0.1:8000/tower.connect.v1.CatalogService
H='-H content-type:application/json -H connect-protocol-version:1'

curl -s $H -d '{"title":"Dune","author":"Herbert","year":1965}' $SVC/AddBook
# {"book":{"author":"Herbert","id":1,"title":"Dune","year":1965}}

curl -s $H -d '{"id":1}' $SVC/GetBook
# {"book":{...}}          (this response is now cached, keyed on {"id":1})

curl -s $H -d '{"id":999}' $SVC/GetBook
# {"code":"not_found","message":"book 999 not found"}   (HTTP 404)
```

## What it demonstrates about the platform

- **Body-hash caching of POST reads** A query is a POST whose body *is*
  the request message; the cache keys the entry on the body hash. `GetBook`
  fragments per id, `ListBooks` (no body read) shares one entry, and a repeated
  identical query returns `304` against its ETag.
- **Derived invalidation across a different surface.** `AddBook`/`DeleteBook`
  emit a `DbWrite`; the platform's write-set analysis purges exactly the cached
  reads of `books` — no cache code in the handlers.
- **The read/write split holds under RPC semantics.** Every method is a POST,
  so the cache config opts POST in (`methods: ["POST"]`); mutating methods
  self-exclude because their `Must DbWrite` poisons the response.

`bench/connect-check.sh` asserts all of the above end-to-end (wire shape, error
envelope + status mapping, per-id body variants, ETag/304, mutation
invalidation) and runs on both the rusqlite and turso builds.

## Caveat: cache pollution on unbounded query bodies

Body-hash caching means an attacker who sends many *distinct* request bodies
mints many distinct cache entries, which can evict the shared/anonymous entries
the cache exists to protect. For a real Connect/GraphQL surface the fix is
**persisted queries** — cache only an allowlist of known request shapes. This
demo is unauthenticated and FIFO-bounded, so it's fine as an illustration, but
production RPC caching should key on a persisted-query id, not the raw body.

## Not implemented (JSON-first scope)

- **Protobuf payloads.** `catalog.proto` is the schema of record; a protobuf
  pass would generate a codec (a wire-format analog to `sqlgen`) and let the
  [connectrpc/conformance](https://github.com/connectrpc/conformance) suite
  audit the server. The caching story is unchanged — the body is opaque bytes
  either way.
- **The Connect GET variant** for side-effect-free methods (HTTP-cacheable via
  standard `GET` + query-encoded message).
- **Streaming**, compression envelopes, deadlines/`connect-timeout-ms`.
- An unknown method returns a plain `404`; full conformance would return an
  `unimplemented` Connect envelope.
