app [routes] { pf: platform "../../platform/main.roc" }

routes : {} -> List({ method : Str, path : Str, handler : Box(Str -> Str) })
routes = |{}| [
    { method: "GET", path: "/", handler: Box.box(|_req| "Hello, world!") },
    { method: "GET", path: "/about", handler: Box.box(|_req| "This is the about page.") },
    { method: "GET", path: "/echo", handler: Box.box(|req| "You sent: ${req}") },
]
