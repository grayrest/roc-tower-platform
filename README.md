# tower-platform

A [Roc](https://www.roc-lang.org/) platform for building web servers, with a
Rust host built on [hyper](https://hyper.rs/), [Tower](https://github.com/tower-rs/tower),
and [matchit](https://github.com/ibraheemdev/matchit) — similar in scope to
Axum, but the application is written in Roc.

**Status: experimental.** Targets the new (Zig-based) Roc compiler built from
source, and tracks its moving platform ABI via generated glue.

## What a Roc app looks like

Handlers are **pure** functions (`->`, not `=>`): the compiler forbids I/O in
their bodies. Inputs arrive as **coeffects** resolved before the handler; writes
leave as **effects** the host runs after it returns.

```roc
app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Cofx

Model : { greeting : Str }

main = |{}| {
    settings: [Port(8000), Compression, CorsPermissive],
    init!: init!,
    routes: [
        # Cofx.withN binds coeffect resolvers to the pure handler's arguments.
        { method: "GET", path: "/", handler: Box.box(Cofx.with1(Req.model, home)) },
        { method: "POST", path: "/greet", handler: Box.box(Cofx.with2(Req.json_body, Req.model, greet)) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    greeting = match Host.env_var!("GREETING") {
        Ok(value) => value
        Err(_) => "Hello"
    }
    Ok({ greeting: greeting })
}

# Pure: `->` arrow. No Host.*! call can appear here — it would not compile.
home : Model -> Response
home = |model| Res.text(200, "${model.greeting}, world!")

# Coeffects resolve before the handler runs, and the handler receives the raw
# values: a bad JSON body never reaches `greet` — the json_body resolver
# rejects it with a 400.
greet : { name : Str }, Model -> Response
greet = |body, model| Res.json(200, { message: "${model.greeting}, ${body.name}" })
```

See [examples/pure/main.roc](examples/pure/main.roc) for the full model —
reserved-id coeffects, a `Must(DbWrite)` effect that runs transactionally with a
`UniqueViolation -> 409` handler, and an `Opt(Log)` — and
[examples/hello/main.roc](examples/hello/main.roc) for a mix of pure handlers
and the effectful escape hatch.

## Architecture

```
Roc app (routes as data, handlers as boxed functions)
        │  generated RustGlue bindings (roc glue)
        ▼
Rust host (staticlib linked by `roc build`)
  ├─ matchit router: path params, 405 + Allow, 404
  ├─ Tower stack: request-id → trace → catch-error → CORS →
  │              compression → concurrency limit → timeout
  ├─ per-request spawn_blocking → erased-callable invoke of the Roc handler
  └─ hosted effects: log, env, time, random, HTTP client (reqwest),
                     SQLite (rusqlite)
```

- **Routing** is declared in Roc as plain data and compiled into a real
  matchit router in the host. Path params (`/users/{id}`) arrive in
  `req.params`.
- **The Model** is produced once by `init!` (fail = abort boot) and shared
  read-only with every handler via `req.model` (atomic refcounts).
- **Handlers run on tokio's blocking pool**, one call per request; the
  interpreter is reentrant (verified with 20k+ concurrent-request stress runs
  under a guarded allocator).
- **Bodies are fully buffered** both ways, guarded by `MaxBodyBytes`
  (413 when exceeded). SSE/WebSockets are out of scope for now.

## Settings

| Setting | Effect |
| --- | --- |
| `Port(U16)` | Listen port (default: `PORT` env var, then 8000) |
| `TimeoutMs(U64)` | Per-request timeout → 408 (default 30s) |
| `MaxBodyBytes(U64)` | Request body cap → 413 (default 2 MiB) |
| `CorsPermissive` | `Access-Control-Allow-Origin: *` |
| `CorsAllowOrigins(List(Str))` | CORS restricted to the given origins |
| `Compression` | gzip/br response compression |
| `ConcurrencyLimit(U64)` | Global in-flight request cap |
| `ServeFiles({ url_prefix, dir })` | Static files served host-side |

## Effects (`pf.Host`)

| Function | Signature |
| --- | --- |
| `log!` | `Str => {}` (routed into the host's `tracing` output) |
| `env_var!` | `Str => Try(Str, [EnvVarNotFound])` |
| `now_ms!` | `() => U64` |
| `random_bytes!` | `U64 => List(U8)` (OS entropy, capped at 1 MiB) |
| `http_send!` | request record `=> Try(response record, Str)` (reqwest, rustls) |
| `sqlite_exec!` | `{ db, sql, params } => Try({ columns, rows }, Str)` |
| `password_hash!` | `Str => Str` (argon2id, PHC string — store this, never the password) |
| `password_verify!` | `Str, Str => Bool` (password, stored PHC string) |

SQL parameters and cells use `Host.SqlValue`:
`[Null, Integer(I64), Real(F64), Text(Str), Blob(List(U8))]`.

## Pure handlers: coeffects and effects (`Cofx.withN`, `Res.fx*`)

The sanctioned handler style. A handler is a **pure** function (`->`) over raw
values: **coeffects** deliver its inputs before it runs, and the `Response` it
returns may carry **effects** the host runs after.

**Coeffects (in).** A coeffect is a **plain function** —
`req => Try(a, Response)`. `Cofx.with1` … `Cofx.with5` bind resolvers to the
handler's positional arguments: each runs left to right (this is where reads
happen — parse a param, load a row, reserve an id), the first `Err` becomes
the response and the handler never runs, otherwise the handler is called with
the raw resolved values — nothing to destructure:

```roc
{ method: "GET", path: "/combo/{id}",
  handler: Box.box(Cofx.with3(Req.param("id"), Req.query_pairs, Req.model, combo)) }

combo : Str, List({ name : Str, value : Str }), Model -> Response
combo = |id, query_pairs, model| ...
```

The reuse story is functions plus a **pipelined** composition surface:

- **Parameterize** by partial application — `Req.param("id")`,
  `paginated(50)`, `require_role("admin")` are functions *returning*
  resolvers.
- **Chain** with the arrow-call operator (`x->f(y)` calls `f(x, y)`) through
  the `Cofx` combinators: `->Cofx.map(…)` transforms a resolved value,
  `->Cofx.resolve(…)` adds a dependent step on the value alone,
  `->Cofx.and_then(…)` a dependent step that gets the value *and* the
  request, and `->Cofx.map_reject(…)` rebrands the rejection response
  (e.g. a built-in 400 into an app's error shape).
- **Combine** independent resolvers into one *named* block with the record
  builder, which desugars through `Cofx.map2` (two or more fields,
  resolved left to right, first rejection wins):

  ```roc
  { method: "GET", path: "/reserve-twice",
    handler: Box.box(Cofx.with1({ a: Req.reserved_id!, b: Req.reserved_id! }.Cofx, reserve_twice)) }

  reserve_twice : { a : I64, b : I64 } -> Response
  ```
- **Reuse** across routes by naming the value.

| Resolver | Resolves to | Rejects |
| --- | --- | --- |
| `Req.body` / `Req.text_body` | raw bytes / UTF-8 body | 400 on bad UTF-8 |
| `Req.json_body` | body decoded into the argument's type | 400 on bad JSON |
| `Req.form_body` | urlencoded pairs | 400 on bad UTF-8 |
| `Req.query_pairs` / `Req.headers` | all query / header pairs | — |
| `Req.param(n)` / `Req.query(n)` / `Req.header(n)` | the named path param / query param / header | 400 on a miss |
| `Req.query_or(n, d)` | the named query param, or `d` when absent | — |
| `Req.model` | the shared app model | — |
| `Req.now!` | the request instant in epoch ms (memoized: one instant per request) | — |
| `Req.reserved_id!` | a fresh DB-reserved id (memoized per request) | 500 on DB error |

An app-defined coeffect is just another function of the same shape; a
dependent read pipelines from a factory (from `examples/pure` — the `{id}`
param resolves, then the row it names loads, with a 404 on a miss):

```roc
fetched_user! : Request => Try({ id : I64, name : Str }, Response)
fetched_user! = Req.param("id")
    ->Cofx.and_then(|id, req| load_user!(id, req))
```

Pure resolvers are unmarked; effectful ones carry the compiler-enforced `!`.
**Memoization is opt-in:** a non-idempotent resolver (or one several
resolvers share) routes its work through `Cofx.get!(req.ctx, key, miss!)` with
a key that includes its parameters, making it run at most once per request —
the two `Req.reserved_id!` fields above share ONE reservation. Pure
projections skip it.

**Effects (out).** A pure handler can't call `Host.*!`; instead it builds a chain
of effects with `Res.fx` / `Res.fx_err` (critical) or `Res.fx_opt`
(fire-and-forget), extends it with `.fx` / `.fx_err` / `.opt`, then finishes with
a response terminal (`.text` / `.json` / `.bytes` / ...) that
folds the effects into the returned `Response`. The host runs them after the
handler: critical effects in a per-request transaction (a `UniqueViolation`/etc.
failure invokes the pure `fx_err` handler for the response) before the response
is sent; `.opt` effects are fire-and-forget and run *after* the response is
flushed, off its critical path — so they add no latency and their result is not
observable by the returning request. Effects come from a closed vocabulary
(`Effect.Tag`): `DbWrite`, `HttpSend`, `Log`.

```roc
create : I64, { name : Str } -> Response
create = |id, body|
    Res.fx_err(
            DbWrite({ db, sql: "INSERT INTO users (id, name) VALUES (?, ?)", params }),
            |err| match err {
                UniqueViolation(_) => { status: 409, headers: [], body: "taken".to_utf8() }
                _ => { status: 500, headers: [], body: "failed".to_utf8() }
            },
        )
        .opt(Log("created ${id.to_str()}"))
        .json(201, { id })
```

**Escape hatch.** Raw effectful handlers (`Request => Response`, no wrapper,
calling `Host.*!` directly) remain fully supported for logic that interleaves
I/O with response-building — see `random!`, `proxy!`, and `db!` in the hello
example. The pure model is the default; this is the exception.

## Helpers (`pf.Res`, `pf.Req`)

Sugar over the structural request/response records:

- **Res**: `text`, `html`, `json` (builtin derive-style `Json.to_str` — works
  on your own records), `bytes`, `no_content`, `redirect`, `with_header`, and
  `from_try : Try(Response, Response) -> Response` for merging error paths.
- **Req**: `get_param`, `get_header` (case-insensitive), `get_query`
  (percent-decoded, `+` as space), `body_str`, `json` (typed `Json.parse` of
  the body), `form` (urlencoded). (The short names `param` / `header` /
  `query` are the route-facing coeffect factories above; all query pairs come
  from the `query_pairs` resolver.)

Fallible `Req` helpers return `Try`; inside a raw handler, delegate to an
inner `Request => Try(Response, Response)` function where `?` works, then
merge with `Res.from_try`.

Three more platform modules round out real JSON APIs over SQL:

- **Sql** — named-column decoding of `Host.sqlite_exec!` results.
  `Sql.first(result)` / `Sql.rows(result)` pair rows with the column names;
  `row.str("title")`, `row.i64("id")`, `row.f64`, `row.bool` (SQLite 0/1
  flags), and `row.nullable_str("bio")` decode cells by NAME into `Try`,
  with the offending column in the error.
- **NullableStr** — a string that is really nullable in JSON. Encodes as
  `null` / `"text"` (the derived encoder would stringify a tag union), and
  decodes both; `Try(NullableStr, [Missing])` distinguishes all three PATCH
  states: absent, explicit `null`, and a value.
- **JsonBool** — a Bool that encodes as JSON `true`/`false`. Temporary: the
  derived encoder currently stringifies `Bool` ("True") even though decode
  works — see notes/2026-07-12-compiler-bool-encode-gap.md; delete once the
  compiler derivation is fixed.

Optional JSON *input* fields decode as `Try(a, [Missing])` — an absent field
parses to `Err(Missing)` instead of failing the body.

## Examples

- **hello** — smallest app; also shows the raw-handler escape hatch.
- **pure** — the coeffect/effect reference: reserved ids, pipelined read
  coeffects, record-builder blocks, transactional writes with err_handlers.
- **e2e**, **ctxmemo** — fixtures for the HTTP and memoization test suites.
- **realworld** — a complete [RealWorld/Conduit](https://realworld-docs.netlify.app)
  backend (19 endpoints: auth, profiles/follows, articles, feed, favorites,
  comments, tags) with every handler pure. Passes the official conformance
  suite (`just realworld-conformance`, 154 requests). Auth is a memoized
  coeffect over opaque tokens; authorization (author-only edits) is a
  resolver; validation rejections ARE the error responses.

## Building

Prerequisites:

- Rust (stable) with cargo
- [just](https://github.com/casey/just) (`brew install just`)
- A `roc` binary built from the compiler's `main` branch
  (`zig build roc` with Zig 0.16 in a roc-lang/roc checkout)

```sh
just build              # examples/hello/main.roc by default
./main                  # or: just run

just build examples/e2e/main.roc
ROC=/path/to/roc just build   # non-default roc checkout
```

`just build` regenerates the Rust glue (`roc glue`), builds the host
staticlib, installs it as the platform's target input, and links the app with
`roc build`. Run `just` alone to list all recipes.

## Tests

```sh
just test-roc    # Res/Req helper expects (roc test)
just test-host   # FFI round-trips under a counting allocator
just test-e2e    # real server over HTTP
just test        # all of the above
just check       # fmt + clippy + all tests

just realworld-conformance   # official RealWorld API suite (needs hurl)
```

## Known limitations

- `crash` in Roc code terminates the process (the erased-callable ABI gives
  the host no safe way to unwind a crashed call). Run under a supervisor and
  prefer returning error responses.
- TLS is a reverse-proxy concern; the host serves HTTP/1.1 + h2c only.
- Rate limiting is not yet exposed (concurrency limiting is).
- Distribution is source-only while the compiler ABI stabilizes; apps
  reference the platform by local path.
- The `requires` block must reference the model as lowercase `model`;
  uppercase `Model` crashes `roc glue` (compiler bug, reported).
