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
                }),
            }),
        }
    }
    exposes [Host, Res, Req, Effect]
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
        "roc_http_send": Host.http_send!,
        "roc_sqlite_exec": Host.sqlite_exec!,
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
