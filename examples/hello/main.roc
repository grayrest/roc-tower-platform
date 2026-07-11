app [main] { pf: platform "../../platform/main.roc" }

import pf.Host

main = |{}| {
    settings: [Port(8000), Compression, CorsPermissive, TimeoutMs(10000), ServeFiles({ url_prefix: "/static", dir: "examples/hello/public" })],
    routes: [
        { method: "GET", path: "/", handler: Box.box(|_req| text(200, "Hello, world!")) },
        { method: "GET", path: "/users/{id}", handler: Box.box(|req| {
            id = match req.params.first() {
                Ok(param) => param.value
                Err(_) => "unknown"
            }
            text(200, "user ${id}")
        }) },
        { method: "POST", path: "/echo", handler: Box.box(|req| {
            status: 200,
            headers: [{ name: "content-type", value: "application/octet-stream" }],
            body: req.body,
        }) },
        { method: "GET", path: "/time", handler: Box.box(|_req| text(200, "now=${Host.now_ms!().to_str()}")) },
    ],
}

text : U16, Str -> { status : U16, headers : List({ name : Str, value : Str }), body : List(U8) }
text = |status, message| {
    status: status,
    headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
    body: Str.to_utf8(message),
}
