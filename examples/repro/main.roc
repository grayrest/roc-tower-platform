app [Model, main] { pf: platform "../../platform/main.roc" }
import pf.Res
import DbGen
Model : {}
main = |{}| { settings: [], init!: || Ok({}), routes: [{ method: "GET", path: "/", handler: Box.box(h!) }] }
h! = |req| {
    _ = DbGen.f!(req.ctx)
    Res.text(200, "ok")
}
