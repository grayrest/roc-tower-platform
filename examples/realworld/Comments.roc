import pf.Req
import pf.Res
import pf.Cofx
import Api
import Auth
import Db
import Articles

## Comments: add to an article, list an article's comments, delete own.
## Article existence checks go through the LIGHT `Db.load_article_meta!`
## (comments never need the article body); `withN` slot order gives the
## spec's 401 → 404 → 422 precedence.
Comments := [].{
    routes = |{ prefix, route }| [
        route("POST", "${prefix}/{slug}/comments", Comments.add_h!),
        route("GET", "${prefix}/{slug}/comments", Comments.list_h!),
        route("DELETE", "${prefix}/{slug}/comments/{id}", Comments.delete_h!),
    ]

    ## Coeffect: the target article's row id — the light existence check,
    ## 404 when the slug is unknown.
    article_id! = Req.param("slug")
        ->Cofx.and_then(|slug, req| {
            meta = Db.load_article_meta!(req.ctx, Db.of(req), slug)?
            Ok(meta.id)
        })

    ## Coeffect: the decoded, validated comment text. Pure.
    comment_body! = |req| {
        body : { comment : { body : Str } }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid comment body")
        Api.nonblank("body", body.comment.body)
    }

    ## `add` bound to its coeffects (auth, article, text, id, instant).
    add_h! = Cofx.bind5(Auth.current_user!, Comments.article_id!, Comments.comment_body!, Req.reserved_id!, Req.now!, Comments.add)

    ## PURE handler: INSERT the comment, answer the comment object as its
    ## author sees it.
    add : Auth.User, I64, Str, I64, U64 -> Res.Response
    add = |author, article_id, text, id, now| {
        stamp = Api.iso8601(now)
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT INTO comments (id, article_id, author_id, body, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                params: [
                    Integer(id),
                    Integer(article_id),
                    Integer(author.id),
                    Text(text),
                    Text(stamp),
                    Text(stamp),
                ],
            }))
            .json(201, {
                comment: {
                    id: id,
                    createdAt: stamp,
                    updatedAt: stamp,
                    body: text,
                    author: Articles.self_profile(author),
                },
            })
    }

    ## Coeffect: the target article's comments as TYPED rows, as the (possibly
    ## anonymous) viewer sees them (following flags); Roc shapes the envelope.
    article_comments! = Comments.article_id!
        ->Cofx.and_then(|article_id, req| {
            viewer = Auth.optional_user!(req)?
            Db.load_comments!(req.ctx, Db.of(req), Auth.viewer_id(viewer), article_id)
        })

    ## `list` bound to its coeffect.
    list_h! = Cofx.bind1(Comments.article_comments!, Comments.list)

    ## PURE handler: serialize the typed comment list as `{comments}` in Roc.
    list : List(Db.Comment) -> Res.Response
    list = |comments| Res.json(200, { comments: comments })

    ## Coeffect: the comment at {slug}/{id}, which the authenticated viewer
    ## must have written — 401 anonymous, 404 unknown article/comment,
    ## 403 not-your-comment.
    owned_comment_id! = |req| {
        user = Auth.current_user!(req)?
        article_id = Comments.article_id!(req)?
        raw_id = Req.get_param(req, "id") ? |_| Fail(400.U16, "body", "missing comment id")
        comment_id = I64.from_str(raw_id) ? |_| Fail(404.U16, "comment", "not found")
        comment = Db.load_comment!(req.ctx, Db.of(req), comment_id, article_id)?
        if comment.author_id == user.id {
            Ok(comment.id)
        } else {
            Err(Fail(403.U16, "comment", "forbidden"))
        }
    }

    ## `delete` bound to the comment-ownership gate.
    delete_h! = Cofx.bind1(Comments.owned_comment_id!, Comments.delete)

    ## PURE handler: delete the comment.
    delete : I64 -> Res.Response
    delete = |comment_id| {
        Res.fx(DbWrite({ db: Db.db_path, sql: "DELETE FROM comments WHERE id = ?", params: [Integer(comment_id)] }))
            .no_content()
    }
}
