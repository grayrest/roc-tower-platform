import pf.Req
import pf.Res
import pf.Cofx
import pf.JsonBool
import Auth
import Db

## Profiles & follows: get profile, follow, unfollow. One reusable read
## coeffect (`target!`); follow/unfollow bind it after the auth slot, so
## 401 precedes 404.
Profiles := [].{
    routes = |{ prefix, route }| [
        route("GET", "${prefix}/{username}", Profiles.get_h!),
        route("POST", "${prefix}/{username}/follow", Profiles.follow_h!),
        route("DELETE", "${prefix}/{username}/follow", Profiles.unfollow_h!),
    ]

    ## Coeffect: the {username} profile with its user id (for follow
    ## writes), as the (possibly anonymous) viewer sees it; 404 unknown.
    target! = Req.param("username")
        ->Cofx.and_then(|username, req| {
            viewer = Auth.optional_user!(req)?
            Db.load_profile!(req.ctx, Db.of(req), Auth.viewer_id(viewer), username)
        })

    ## `get` bound to the typed `target!` read.
    get_h! = Cofx.bind1(Profiles.target!, Profiles.get)

    ## PURE handler: serialize the typed profile as `{profile}` in Roc.
    get : { id : I64, profile : Db.Profile } -> Res.Response
    get = |target| Res.json(200, { profile: target.profile })

    ## `follow` / `unfollow` bound to their coeffects.
    follow_h! = Cofx.bind2(Auth.current_user!, Profiles.target!, Profiles.follow)

    unfollow_h! = Cofx.bind2(Auth.current_user!, Profiles.target!, Profiles.unfollow)

    ## PURE handler: record the follow (idempotent), answer the profile
    ## with following flipped on.
    follow : Auth.User, { id : I64, profile : Db.Profile } -> Res.Response
    follow = |user, target| {
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
                params: [Integer(user.id), Integer(target.id)],
            }))
            .json(200, { profile: { ..target.profile, following: JsonBool.from(True) } })
    }

    ## PURE handler: remove the follow (idempotent), answer the profile
    ## with following flipped off.
    unfollow : Auth.User, { id : I64, profile : Db.Profile } -> Res.Response
    unfollow = |user, target| {
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "DELETE FROM follows WHERE follower_id = ? AND followed_id = ?",
                params: [Integer(user.id), Integer(target.id)],
            }))
            .json(200, { profile: { ..target.profile, following: JsonBool.from(False) } })
    }
}
