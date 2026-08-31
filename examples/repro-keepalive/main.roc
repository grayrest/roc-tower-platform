# Minimal reproduction of a Roc codegen use-after-free / bad-decref that crashes
# the tower-platform host under HTTP keep-alive load (and, in a --debug build, on
# the very first request). See README.md for the full analysis.
#
# The whole trigger is a single route whose handler returns a Res.Response: on
# return, generated code decrefs a value that is intermittently a non-pointer
# (~= sizeof the request record), faulting in roc_llvm_rc_decref. No DB, no
# coeffects, no JSON encoding are required — this is as small as it gets.
app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Res

Model : {}

Request : {
    ctx : U64,
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    model : Box(Model),
}

main = |{}| {
    settings: [TimeoutMs(10000)],
    init!: init!,
    routes: [{ method: "GET", path: "/ping", handler: Box.box(ping!) }],
}

init! : () => Try(Model, Str)
init! = || Ok({})

## A bare `Request => Res.Response` handler. Returning ANY response is enough to
## trigger the fault; `Res.json` with a small list is used only to mirror the
## realworld endpoint the bug was first seen on.
ping! : Request => Res.Response
ping! = |_req| Res.json(200, { tags: ["react", "angular", "vue"] })
