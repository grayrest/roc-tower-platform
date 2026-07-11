platform ""
    requires {
        routes : {} -> List({ method : Str, path : Str, handler : Box(Str => Str) })
    }
    exposes [Host]
    packages {}
    provides { "roc_routes": routes_for_host }
    hosted {
        "roc_now_ms": Host.now_ms!,
    }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
        x64mac: { inputs: ["libhost.a", app] },
    }

import Host

routes_for_host : {} -> List({ method : Str, path : Str, handler : Box(Str => Str) })
routes_for_host = |{}| routes({})
