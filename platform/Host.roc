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

    ## Execute a SQL statement against a SQLite database file.
    ## Returns result columns and rows (both empty for statements like INSERT).
    sqlite_exec! : {
        db : Str,
        sql : Str,
        params : List(SqlValue),
    } => Try({
        columns : List(Str),
        rows : List(List(SqlValue)),
    }, Str)

    ## A non-idempotent effect used by the coeffect layer's tests and the
    ## `ReservedId`-style coeffect: each call increments a process-global
    ## counter and returns the new value. Stands in for a "reserve an id" style
    ## coeffect that must run exactly once per request.
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
