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
}
