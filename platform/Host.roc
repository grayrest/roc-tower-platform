Host := [].{
    ## A SQLite value for statement parameters and result cells.
    SqlValue := [Null, Integer(I64), Real(F64), Text(Str), Blob(List(U8))]

    ## Log a message at info level through the host's structured logger.
    log! : Str => {}

    ## Read an environment variable.
    env_var! : Str => Try(Str, [EnvVarNotFound])

    ## Milliseconds since the Unix epoch.
    now_ms! : () => U64

    ## Cryptographically secure random bytes.
    random_bytes! : U64 => List(U8)

    ## Hash a password with argon2id and a fresh random salt; returns a PHC
    ## string (`$argon2id$...`) safe to store. Store only this, never the
    ## password.
    password_hash! : Str => Str

    ## Verify a password against a stored PHC hash string. False on mismatch
    ## or when the stored string is not a valid PHC hash.
    password_verify! : Str, Str => Bool

    ## Send an outbound HTTP request and wait for the response.
    http_send! : {
        method : Str,
        url : Str,
        headers : List({ name : Str, value : Str }),
        body : List(U8),
        timeout_ms : U64,
    } => Try({
        status : U16,
        headers : List({ name : Str, value : Str }),
        body : List(U8),
    }, Str)

    ## The storage class of a live cursor cell (`sql_col_type!`). Mirrors
    ## SQLite's `sqlite3_column_type`; drives the by-name dynamic decode in
    ## `Sql.roc` and lets generated readers assert a NOT-NULL column isn't NULL.
    ColType := [Null, Integer, Real, Text, Blob]

    ## Prepare a statement against a pooled connection and step to just before
    ## the first row. `ctx` is the request's coeffect ctx (0 at init time); the
    ## cursor is registered under it and swept if the request tears down without
    ## `sql_finish!`. Returns an opaque cursor handle. Write statements are
    ## accepted (the reserved-id coeffect runs `UPDATE ... RETURNING` here).
    sql_begin! : {
        ctx : U64,
        db : Str,
        sql : Str,
        params : List(SqlValue),
    } => Try(U64, Str)

    ## Advance a cursor. `Ok(Bool.true)` when a row is available for the
    ## `sql_col_*!` getters, `Ok(Bool.false)` at end of results.
    sql_step! : U64 => Try(Bool, Str)

    ## The column names of a cursor's result, in order (for by-name decode).
    sql_columns! : U64 => List(Str)

    ## The storage class of column `index` in the current row.
    sql_col_type! : U64, U64 => Host.ColType

    ## Column `index` of the current row as I64 (SQLite coerces per affinity;
    ## NULL reads as 0 — check `sql_col_type!` first when it may be NULL).
    sql_col_i64! : U64, U64 => I64

    ## Column `index` of the current row as F64.
    sql_col_f64! : U64, U64 => F64

    ## Column `index` of the current row as Str (NULL and non-text coerce).
    sql_col_str! : U64, U64 => Str

    ## Step a cursor, returning a per-row GENERATION TOKEN (H1). `Ok(0)` means
    ## Done (no more rows); `Ok(tok)` with `tok > 0` is a live row whose cells
    ## are read by `sql_row_str` etc. The token is a process-global monotonic
    ## id, minted fresh on every advancing step, so it names ONE step event and
    ## nothing else ever carries it. Effectful: it runs the VDBE and advances.
    sql_step_row! : U64 => Try(U64, Str)

    ## Read column `index` of the row named by `tok` as Str — the PURE
    ## companion to `sql_col_str!` (rung 0 + H1). It is a genuine function: the
    ## token identifies a specific step event, and the host hard-errors if the
    ## cursor has since advanced past it (generation mismatch), so it can never
    ## silently return a different row's cell. Because `tok` is loop-carried and
    ## data-dependent on `sql_step_row!`, no optimizer pass can hoist or CSE a
    ## read across a step — the purity is sound by SSA dataflow, not convention.
    ## Typed pure (`->`), which trips the `effectful function name` warning on a
    ## hosted declaration; that is expected until the compiler can express
    ## deliberate purity (note rung 1).
    sql_row_str : U64, U64, U64 -> Str

    ## Column `index` of the current row as a byte blob.
    sql_col_blob! : U64, U64 => List(U8)

    ## Finalize a cursor and return its connection to the pool. Idempotent: a
    ## second finish, or a finish of an unknown handle, is a no-op.
    sql_finish! : U64 => {}

    ## Reserve the next persistent monotonic id for the named database — the
    ## real "reserve an id" primitive backing `Req.reserved_id!`. The host owns a
    ## per-database sequence (a `counters` row it lazily creates); each call bumps
    ## and returns it. A DB failure panics the request (→ 500). No app-visible SQL
    ## write, so the read/write split stays clean.
    next_id! : Str => I64

    ## The raw request body bytes for the request ctx. The body is NOT a
    ## field of the request record: extraction is host-mediated so the
    ## response cache knows which routes vary on the body — an extracting
    ## route gains a body-hash variant dimension; a non-extracting route
    ## caches regardless of what a client sent (decision log D14). ctx 0
    ## (init time) has no body: returns empty.
    request_body! : U64 => List(U8)

    ## Override the request's body-variant cache key with a CANONICAL key —
    ## the structural hash of the DECODED body value, computed by the typed
    ## body extractor (`Req.json_body!`). Two raw spellings of the same
    ## message (whitespace, key order, junk fields) then share one cached
    ## entry instead of minting duplicates (the cache-DoS mitigation). The
    ## host applies it only when every `request_body!` read was paired with a
    ## key (a RAW body read wins — byte-equality is all that's safe then).
    set_body_key! : U64, U64 => {}

    ## Override the request's QUERY-variant cache key with a CANONICAL key —
    ## the structural hash of the DECODED query record (`Req.query_record!`).
    ## Two raw spellings of the same query (extra junk params, order) then
    ## share one cached entry: the host aliases the raw request target to a
    ## canonical one. Only a route that DECODES its query reports a key; a
    ## route that never reads the query is unaffected (no drop — fail-safe).
    set_query_key! : U64, U64 => {}

    ## The process-wide random seed for canonical body-key hashing. Seeding
    ## `to_hash` makes canonical-key collisions non-craftable offline; the
    ## in-memory cache only needs in-process stability.
    hash_seed! : () => U64

    ## The named request cookie's value for the request ctx (RFC 6265
    ## request-cookie parsing; first match wins). The Cookie header is
    ## STRIPPED from the request record's header list: cookie access is
    ## host-mediated so cached responses can vary on exactly the cookie
    ## names a route reads (decision log D13).
    cookie! : U64, Str => Try(Str, [NoCookie])

    ## A non-idempotent in-memory counter used by the coeffect layer's tests: each
    ## call increments a process-global counter and returns the new value. Kept
    ## distinct from `next_id!` (which is persistent + per-database) so the memo
    ## tests can assert exact injector run counts via `reserve_runs!`.
    reserve_id! : () => U64

    ## How many times `reserve_id!` has actually run, process-wide. Used by the
    ## coeffect memoization tests to assert exact injector run counts.
    reserve_runs! : () => U64

    ## Per-request heterogeneous coeffect memo store. The first argument is the
    ## host-minted request ctx id (`req.ctx`); each id names an isolated store
    ## torn down when its request completes. Values cross the FFI as boxed
    ## THUNKS (`Box({} => a)`), not raw value boxes: an erased callable has a
    ## fixed allocation layout plus a compiler-generated capture-drop, so the
    ## host can release it correctly at teardown, which is impossible for an
    ## opaque `Box(a)` (freeing needs the payload's elements_refcounted flag and
    ## alignment, which the FFI erases). On hit returns the stored thunk; on
    ## miss returns `Err(Miss)`. Invoking the thunk is an unchecked downcast, so
    ## keys must uniquely identify the type within one request.
    memo_get! : U64, Str => Try(Box(({} => a)), [Miss])

    ## Store a boxed thunk under a key in the request's coeffect store.
    memo_put! : U64, Str, Box(({} => a)) => {}

    ## Debug: how many request stores are currently live. The request asking is
    ## itself live, so a quiescent server reports 1.
    ctx_registry_size! : () => U64

    ## Debug: how many boxes all request stores currently retain. Balanced
    ## teardown means this is 0 whenever no request holds memos.
    memo_live_boxes! : () => U64

    ## Test-only: panic on the host side to prove Drop-guard teardown survives
    ## panic-unwind out of a hosted call.
    panic_test! : () => {}
}
