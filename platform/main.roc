platform ""
    requires {
        main : {} -> {
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
                } => {
                    status : U16,
                    headers : List({ name : Str, value : Str }),
                    body : List(U8),
                }),
            }),
        }
    }
    exposes [Host]
    packages {}
    provides { "roc_app": main_for_host }
    hosted {
        "roc_now_ms": Host.now_ms!,
    }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
        x64mac: { inputs: ["libhost.a", app] },
    }

import Host

main_for_host = |{}| main({})
