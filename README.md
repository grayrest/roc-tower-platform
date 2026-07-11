# tower-platform

A [Roc](https://www.roc-lang.org/) platform for building web servers, with a
Rust host built on [hyper](https://hyper.rs/), [Tower](https://github.com/tower-rs/tower),
and [matchit](https://github.com/ibraheemdev/matchit) — similar in scope to
Axum, but the application is written in Roc.

**Status: experimental.** Targets the new (Zig-based) Roc compiler built from
source, and tracks its moving platform ABI via generated glue.

## What a Roc app looks like

```roc
app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req

Model : { greeting : Str }

main = |{}| {
    settings: [Port(8000), Compression, CorsPermissive],
    init!: init!,
    routes: [
        { method: "GET", path: "/", handler: Box.box(Req.args1(home!)) },
        { method: "POST", path: "/greet", handler: Box.box(Req.args2(greet!)) },
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

home! : Req.Model(Model) => Response
home! = |model_arg| Res.text(200, "${model_arg.value().greeting}, world!")

## Handler arguments are extracted from the request by type: a bad JSON body
## never reaches the handler — the extractor rejects it with a 400.
greet! : Req.JsonBody({ name : Str }), Req.Model(Model) => Response
greet! = |body_arg, model_arg| {
    Res.json(200, { message: "${model_arg.value().greeting}, ${body_arg.value().name}" })
}
```

See [examples/hello/main.roc](examples/hello/main.roc) for the full version,
including path params, SQLite, and outbound HTTP.

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

SQL parameters and cells use `Host.SqlValue`:
`[Null, Integer(I64), Real(F64), Text(Str), Blob(List(U8))]`.

## Handlers and extractors (`Req.argsN`)

The blessed handler style: declare typed arguments, and each argument type's
`from_request` method pulls it out of the request. Wrap the handler with
`Req.args1` … `Req.args5` (matching its arity) in the route list:

```roc
{ method: "GET", path: "/combo/{id}", handler: Box.box(Req.args3(combo!)) }

combo! : UserId, Req.QueryParams, Req.Model(Model) => Response
combo! = |id_arg, query_arg, model_arg| ...
```

Arguments extract left to right; the first `Err` rejection is returned as the
response and the handler never runs.

Platform extractors (all under `Req`):

| Type | Extracts | Rejects |
| --- | --- | --- |
| `Req.Body` | raw body bytes | never |
| `Req.BodyStr` | body as UTF-8 text | 400 on invalid UTF-8 |
| `Req.JsonBody(a)` | body decoded as JSON into `a` | 400 on bad JSON |
| `Req.FormBody` | urlencoded form pairs | 400 on invalid UTF-8 |
| `Req.QueryParams` | decoded query pairs | never |
| `Req.Headers` | request headers | never |
| `Req.Model(m)` | the shared app model, unboxed | never |

Each wraps its payload: `.value()` returns it, and `Req.FormBody`,
`Req.QueryParams`, and `Req.Headers` also have `.get(name)` (headers
case-insensitive).

Custom extractors are nominal types with a `from_request` method — this is
how path parameters get names:

```roc
UserId := [UserId(Str)].{
    from_request : Request -> Try(UserId, Response)
    from_request = |req| {
        match Req.param(req, "id") {
            Ok(value) => Ok(UserId.UserId(value))
            Err(_) => Err(Res.text(400, "missing id path parameter"))
        }
    }
}
```

Raw handlers (`Request => Response`, no wrapper) remain fully supported —
see `random!`, `proxy!`, and `db!` in the hello example.

Note: multi-argument handler *types* are written `(a, b => resp)` — the arrow
inside the parens. `(a, b) => resp` is a function taking one tuple.

## Helpers (`pf.Res`, `pf.Req`)

Sugar over the structural request/response records:

- **Res**: `text`, `html`, `json` (builtin derive-style `Json.to_str` — works
  on your own records), `bytes`, `no_content`, `redirect`, `with_header`, and
  `from_try : Try(Response, Response) -> Response` for merging error paths.
- **Req**: `param`, `header` (case-insensitive), `query_params` / `query`
  (percent-decoded, `+` as space), `body_str`, `json` (typed `Json.parse` of
  the body), `form` (urlencoded).

Fallible `Req` helpers return `Try`; inside a raw handler, delegate to an
inner `Request => Try(Response, Response)` function where `?` works, then
merge with `Res.from_try`.

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
