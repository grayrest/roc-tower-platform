import pf.Req
import pf.Res
import pf.Cofx
import Api
import Auth
import Db
import Articles

## Comments: add to an article, list an article's comments, delete own.
Comments := [].{
    routes = |{}| [
        { method: "POST", path: "/api/articles/{slug}/comments", handler: Box.box(Cofx.with1(Comments.add_input!, Comments.add)) },
        { method: "GET", path: "/api/articles/{slug}/comments", handler: Box.box(Cofx.with1(Comments.list_input!, Comments.list)) },
        { method: "DELETE", path: "/api/articles/{slug}/comments/{id}", handler: Box.box(Cofx.with1(Comments.delete_input!, Comments.delete)) },
    ]

    ## Coeffect: author, target article (404), decoded body, a reserved
    ## comment id, and the request instant.
    add_input! = |req| {
        author = Auth.current_user!(req)?
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        loaded = Db.load_article!(Box.unbox(req.model).db, author.id, slug)?
        body : { comment : { body : Str } }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid comment body")
        text = Api.nonblank("body", body.comment.body)?
        id = Req.reserved_id!(req)?
        now = Req.now!(req)?
        Ok({ author: author, article_id: loaded.id, text: text, id: id, now: now })
    }

    ## PURE handler: INSERT the comment, answer the comment object as its
    ## author sees it.
    add = |input| {
        stamp = Api.iso8601(input.now)
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT INTO comments (id, article_id, author_id, body, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                params: [
                    Integer(input.id),
                    Integer(input.article_id),
                    Integer(input.author.id),
                    Text(input.text),
                    Text(stamp),
                    Text(stamp),
                ],
            }))
            .json(201, {
                comment: {
                    id: input.id,
                    createdAt: stamp,
                    updatedAt: stamp,
                    body: input.text,
                    author: Articles.self_profile(input.author),
                },
            })
    }

    ## Coeffect: the target article's comments as the (possibly anonymous)
    ## viewer sees them (following flags).
    list_input! = |req| {
        viewer = Auth.optional_user!(req)?
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        db = Box.unbox(req.model).db
        loaded = Db.load_article!(db, Auth.viewer_id(viewer), slug)?
        Db.load_comments!(db, Auth.viewer_id(viewer), loaded.id)
    }

    ## PURE handler: the comments envelope.
    list : List(Db.Comment) -> Res.Response
    list = |comments| Res.json(200, { comments: comments })

    ## Coeffect: the comment at {slug}/{id}, which the authenticated viewer
    ## must have written: 401 anonymous, 404 unknown, 403 not-your-comment.
    delete_input! = |req| {
        user = Auth.current_user!(req)?
        slug = Req.get_param(req, "slug") ? |_| Api.error(400, "missing slug")
        id_raw = Req.get_param(req, "id") ? |_| Api.error(400, "missing comment id")
        comment_id = I64.from_str(id_raw) ? |_| Api.not_found("comment")
        db = Box.unbox(req.model).db
        loaded = Db.load_article!(db, user.id, slug)?
        comment = Db.load_comment!(db, comment_id, loaded.id)?
        if comment.author_id == user.id {
            Ok(comment.id)
        } else {
            Err(Api.forbidden("comment"))
        }
    }

    ## PURE handler: delete the comment.
    delete : I64 -> Res.Response
    delete = |comment_id| {
        Res.fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM comments WHERE id = ?", params: [Integer(comment_id)] }))
            .no_content()
    }
}
