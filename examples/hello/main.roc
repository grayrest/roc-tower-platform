app [routes] { pf: platform "../../platform/main.roc" }

import pf.Host

routes : {} -> List({ method : Str, path : Str, handler : Box(Str => Str) })
routes = |{}| [
    { method: "GET", path: "/", handler: Box.box(|_req| "Hello, world!") },
    { method: "GET", path: "/about", handler: Box.box(|_req| "This is the about page.") },
    { method: "GET", path: "/time", handler: Box.box(|_req| "now=${Host.now_ms!().to_str()}") },
]
