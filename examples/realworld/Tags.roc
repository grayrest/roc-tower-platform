import pf.Res
import pf.Cofx
import pf.RawJson
import Api
import Db

## Tags: the one read-only listing endpoint (`GET /api/tags`). Its own module
## so the `Api.read_mode` toggle lives in a nominal block (main.roc's top-level
## defs are compile-time evaluated, which flags the unused const branch).
Tags := [].{
    routes = |{ prefix }| [
        { method: "GET", path: prefix, handler: Tags.get_h! },
    ]

    ## Coeffect: the tags envelope, assembled in SQL.
    tags_json! = |req| Db.all_tags_json!(req.ctx, Box.unbox(req.model).db)

    ## Coeffect: the tags as a typed `List(Str)` (Roc shapes the envelope).
    tags_typed! = |req| Db.all_tags!(req.ctx, Box.unbox(req.model).db)

    ## `get` bound to its coeffect. `Typed` serializes `{tags}` in Roc; `Json`
    ## streams the SQL-assembled document verbatim.
    get_h! =
        if Api.read_mode == Typed
            Box.box(Cofx.with1(Tags.tags_typed!, Tags.get_typed))
        else
            Box.box(Cofx.with1(Tags.tags_json!, Tags.get))

    ## PURE handler (Json mode): the SQL-assembled tags envelope, verbatim.
    get : RawJson -> Res.Response
    get = |doc| Res.json_raw(200, doc)

    ## PURE handler (Typed mode): serialize the typed tag list in Roc.
    get_typed : List(Str) -> Res.Response
    get_typed = |tag_list| Res.json(200, { tags: tag_list })
}
