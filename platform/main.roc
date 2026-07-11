platform ""
    requires {
        handle! : Str => Str
    }
    exposes []
    packages {}
    provides { "roc_handle": handle_for_host! }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
        x64mac: { inputs: ["libhost.a", app] },
    }

handle_for_host! : Str => Str
handle_for_host! = |req| handle!(req)
