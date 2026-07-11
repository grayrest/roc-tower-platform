platform ""
    requires {
        routes : {} -> List({ method : Str, path : Str, handler : Box(Str -> Str) })
    }
    exposes []
    packages {}
    provides { "roc_routes": routes_for_host }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
        x64mac: { inputs: ["libhost.a", app] },
    }

routes_for_host : {} -> List({ method : Str, path : Str, handler : Box(Str -> Str) })
routes_for_host = |{}| routes({})
