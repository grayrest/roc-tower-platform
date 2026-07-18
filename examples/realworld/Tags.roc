import pf.Res
import pf.Cofx
import Db

## Tags: the one read-only listing endpoint (`GET /api/tags`).
Tags := [].{
    routes = |{ prefix, route }| [
        route("GET", prefix, Tags.get_h!),
    ]

    ## Coeffect: the tags as a typed `List(Str)` (Roc shapes the envelope).
    tags! = |req| Db.all_tags!(req.ctx, Box.unbox(req.model).db)

    ## `get` bound to its coeffect.
    get_h! = Cofx.bind1(Tags.tags!, Tags.get)

    ## PURE handler: serialize the tag list as `{tags}` in Roc. Cacheable —
    ## purged whenever `article_tags` is written.
    get : List(Str) -> Res.Response
    get = |tag_list| Res.json(200, { tags: tag_list })
}
