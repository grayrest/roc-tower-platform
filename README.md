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
        { method: "GET", path: "/", handler: Box.box(home!) },
        { method: "POST", path: "/greet", handler: Box.box(greet!) },
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

home! : Request => Response
home! = |req| Res.text(200, "${Box.unbox(req.model).greeting}, world!")

greet! : Request => Response
greet! = |req| {
    parsed : Try({ name : Str }, _)
    parsed = Req.json(req)
    match parsed {
        Ok(body) => Res.json(200, { message: "hello, ${body.name}" })
        Err(_) => Res.text(400, "expected {\"name\":...}")
    }
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

## Helpers (`pf.Res`, `pf.Req`)

Optional sugar over the structural request/response records:

- **Res**: `text`, `html`, `json` (builtin derive-style `Json.to_str` — works
  on your own records), `bytes`, `no_content`, `redirect`, `with_header`, and
  `from_try : Try(Response, Response) -> Response` for merging error paths.
- **Req**: `param`, `header` (case-insensitive), `query_params` / `query`
  (percent-decoded, `+` as space), `body_str`, `json` (typed `Json.parse` of
  the body), `form` (urlencoded).

Fallible `Req` helpers return `Try`; delegate to an inner
`Request => Try(Response, Response)` function where `?` works, then merge with
`Res.from_try`.

## Building

Prerequisites:

- Rust (stable) with cargo
- A `roc` binary built from the compiler's `main` branch
  (`zig build roc` with Zig 0.16 in a roc-lang/roc checkout)

```sh
ROC=~/Repositories/roc/zig-out/bin/roc ./scripts/build-example.sh examples/hello/main.roc
./main
```

The script regenerates the Rust glue (`roc glue`), builds the host staticlib,
installs it as the platform's target input, and links the app with
`roc build`.

## Tests

```sh
roc test platform/main.roc          # Res/Req helper expects
cargo test -p tower-platform-host   # FFI round-trips under a counting allocator
cargo test -p tower-platform-e2e    # real server over HTTP (needs ROC)
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
