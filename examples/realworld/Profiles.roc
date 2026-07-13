import pf.Req
import pf.Res
import pf.Cofx
import pf.JsonBool
import Api
import Auth
import Db

## Profiles & follows: get profile, follow, unfollow. One reusable read
## coeffect (`target!`); follow/unfollow bind it after the auth slot, so
## 401 precedes 404.
Profiles := [].{
    routes = |{ prefix }| [
        { method: "GET", path: "${prefix}/{username}", handler: Profiles.get_h! },
        { method: "POST", path: "${prefix}/{username}/follow", handler: Profiles.follow_h! },
        { method: "DELETE", path: "${prefix}/{username}/follow", handler: Profiles.unfollow_h! },
    ]

    ## Coeffect: the {username} profile with its user id (for follow
    ## writes), as the (possibly anonymous) viewer sees it; 404 unknown.
    target! = Req.param("username")
        ->Cofx.and_then(|username, req| {
            viewer = Auth.optional_user!(req)?
            Db.load_profile!(Db.of(req), Auth.viewer_id(viewer), username)
        })

    ## Coeffect: just the profile (GET's whole input).
    profile! = Profiles.target!->Cofx.map(|target| target.profile)

    ## `get` bound to its coeffect.
    get_h! = Box.box(Cofx.with1(Profiles.profile!, Profiles.get))

    ## PURE handler: the profile envelope.
    get : Db.Profile -> Res.Response
    get = |profile| Res.json(200, { profile: profile })

    ## `follow` / `unfollow` bound to their coeffects.
    follow_h! = Box.box(Cofx.with2(Auth.current_user!, Profiles.target!, Profiles.follow))

    unfollow_h! = Box.box(Cofx.with2(Auth.current_user!, Profiles.target!, Profiles.unfollow))

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
