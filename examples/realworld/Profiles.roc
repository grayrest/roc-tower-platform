import pf.Req
import pf.Res
import pf.Cofx
import pf.JsonBool
import Api
import Auth
import Db

## Profiles & follows: get profile, follow, unfollow.
Profiles := [].{
    routes = |{}| [
        { method: "GET", path: "/api/profiles/{username}", handler: Box.box(Cofx.with1(Profiles.profile!, Profiles.get)) },
        { method: "POST", path: "/api/profiles/{username}/follow", handler: Box.box(Cofx.with1(Profiles.follow_input!, Profiles.follow)) },
        { method: "DELETE", path: "/api/profiles/{username}/follow", handler: Box.box(Cofx.with1(Profiles.follow_input!, Profiles.unfollow)) },
    ]

    ## Coeffect: the named profile as the (possibly anonymous) viewer sees
    ## it — following flag included; 404 when the username is unknown.
    profile! = |req| {
        username = Req.get_param(req, "username") ? |_| Api.error(400, "missing username")
        viewer = Auth.optional_user!(req)?
        loaded = Db.load_profile!(Box.unbox(req.model).db, Auth.viewer_id(viewer), username)?
        Ok(loaded.profile)
    }

    ## PURE handler: the profile envelope.
    get : Db.Profile -> Res.Response
    get = |profile| Res.json(200, { profile: profile })

    ## Coeffect: an authenticated viewer plus the target profile (with its
    ## user id, for the follows write).
    follow_input! = |req| {
        viewer = Auth.current_user!(req)?
        username = Req.get_param(req, "username") ? |_| Api.error(400, "missing username")
        loaded = Db.load_profile!(Box.unbox(req.model).db, viewer.id, username)?
        Ok({ viewer_id: viewer.id, target_id: loaded.id, profile: loaded.profile })
    }

    ## PURE handler: record the follow (idempotent), answer the profile
    ## with following flipped on.
    follow = |input| {
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
                params: [Integer(input.viewer_id), Integer(input.target_id)],
            }))
            .json(200, { profile: { ..input.profile, following: JsonBool.from(True) } })
    }

    ## PURE handler: remove the follow (idempotent), answer the profile
    ## with following flipped off.
    unfollow = |input| {
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "DELETE FROM follows WHERE follower_id = ? AND followed_id = ?",
                params: [Integer(input.viewer_id), Integer(input.target_id)],
            }))
            .json(200, { profile: { ..input.profile, following: JsonBool.from(False) } })
    }
}
