platform ""
    requires {
        [Model : model] for main : {} -> {
            settings : List([
                Port(U16),
                TimeoutMs(U64),
                MaxBodyBytes(U64),
                CorsPermissive,
                CorsAllowOrigins(List(Str)),
                Compression,
                ConcurrencyLimit(U64),
                ServeFiles({ url_prefix : Str, dir : Str }),
            ]),
            init! : () => Try(model, Str),
            routes : List({
                method : Str,
                path : Str,
                handler : Box({
                    ctx : U64,
                    method : Str,
                    path : Str,
                    params : List({ name : Str, value : Str }),
                    query : Str,
                    headers : List({ name : Str, value : Str }),
                    body : List(U8),
                    model : Box(model),
                } => {
                    status : U16,
                    headers : List({ name : Str, value : Str }),
                    body : List(U8),
                    effects : List([
                        Must([
                            DbWrite({ db : Str, sql : Str, params : List(Host.SqlValue) }),
                            HttpSend({
                                method : Str,
                                url : Str,
                                headers : List({ name : Str, value : Str }),
                                body : List(U8),
                                timeout_ms : U64,
                            }),
                            Log(Str),
                        ], Box([
                            UniqueViolation(Str),
                            ForeignKeyViolation(Str),
                            Busy,
                            HttpFailed(Str),
                            Other(Str),
                        ] -> {
                            status : U16,
                            headers : List({ name : Str, value : Str }),
                            body : List(U8),
                        })),
                        Opt([
                            DbWrite({ db : Str, sql : Str, params : List(Host.SqlValue) }),
                            HttpSend({
                                method : Str,
                                url : Str,
                                headers : List({ name : Str, value : Str }),
                                body : List(U8),
                                timeout_ms : U64,
                            }),
                            Log(Str),
                        ]),
                    ]),
                }),
            }),
        }
    }
    exposes [Host, Res, Req, Effect, Cofx, Sql, NullableStr, JsonBool]
    packages {}
    provides {
        "roc_app": app_for_host,
        "roc_init": init_for_host!,
    }
    hosted {
        "roc_log": Host.log!,
        "roc_env_var": Host.env_var!,
        "roc_now_ms": Host.now_ms!,
        "roc_random_bytes": Host.random_bytes!,
        "password_hash": Host.password_hash!,
        "password_verify": Host.password_verify!,
        "roc_http_send": Host.http_send!,
        "roc_sql_begin": Host.sql_begin!,
        "roc_sql_step": Host.sql_step!,
        "roc_sql_columns": Host.sql_columns!,
        "roc_sql_col_type": Host.sql_col_type!,
        "roc_sql_col_i64": Host.sql_col_i64!,
        "roc_sql_col_f64": Host.sql_col_f64!,
        "roc_sql_col_str": Host.sql_col_str!,
        "roc_sql_col_blob": Host.sql_col_blob!,
        "roc_sql_finish": Host.sql_finish!,
        "roc_reserve_id": Host.reserve_id!,
        "roc_reserve_runs": Host.reserve_runs!,
        "roc_memo_get": Host.memo_get!,
        "roc_memo_put": Host.memo_put!,
        "roc_ctx_registry_size": Host.ctx_registry_size!,
        "roc_memo_live_boxes": Host.memo_live_boxes!,
        "roc_panic_test": Host.panic_test!,
    }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
        x64mac: { inputs: ["libhost.a", app] },
    }

import Host
import Res
import Req
import Effect
import Cofx
import Sql
import NullableStr
import JsonBool

app_for_host = |{}| {
    program = main({})
    { settings: program.settings, routes: program.routes }
}

init_for_host! : () => Try(Box(Model), Str)
init_for_host! = || {
    program = main({})
    match (program.init!)() {
        Ok(model) => Ok(Box.box(model))
        Err(message) => Err(message)
    }
}
