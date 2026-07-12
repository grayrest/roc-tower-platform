import pf.Host
import pf.Req
import pf.Res
import pf.Cofx
import pf.Sql
import pf.NullableStr
import Api
import Auth
import Db

## Users & authentication: register, login, current user, update user.
Users := [].{
    routes = |{}| [
        { method: "POST", path: "/api/users", handler: Box.box(Cofx.with1(Users.registration!, Users.register)) },
        { method: "POST", path: "/api/users/login", handler: Box.box(Cofx.with1(Users.credentials!, Users.login)) },
        { method: "GET", path: "/api/user", handler: Box.box(Cofx.with1(Auth.current_user!, Users.current)) },
        { method: "PUT", path: "/api/user", handler: Box.box(Cofx.with1(Users.update_input!, Users.update)) },
    ]

    ## The RealWorld user envelope every users endpoint responds with.
    user_body : Auth.User -> { user : { email : Str, token : Str, username : Str, bio : NullableStr, image : NullableStr } }
    user_body = |user| {
        user: {
            email: user.email,
            token: user.token,
            username: user.username,
            bio: user.bio,
            image: user.image,
        },
    }

    ## Coeffect: everything registration needs — decoded body, a reserved
    ## user id, a fresh token, and the password hashed host-side (a READ:
    ## hashing must happen before the pure handler).
    registration! = |req| {
        body : { user : { username : Str, email : Str, password : Str } }
        body = Req.json_body(req) ? |_| Api.unprocessable("invalid registration body")
        id = Req.reserved_id!(req)?
        token = Auth.fresh_token!(req)?
        hash = Host.password_hash!(body.user.password)
        Ok({ id: id, email: body.user.email, username: body.user.username, hash: hash, token: token })
    }

    ## PURE handler: INSERT the user and their first token in one
    ## transaction; a duplicate email/username fails the Must and the
    ## err_handler answers 422.
    register : { id : I64, email : Str, username : Str, hash : Str, token : Str } -> Res.Response
    register = |input| {
        on_error = |err| match err {
            UniqueViolation(_) => Api.reply(422, "username or email has already been taken")
            _ => Api.reply(500, "registration failed")
        }
        Res.fx_err(
                DbWrite({
                    db: Db.db_path,
                    sql: "INSERT INTO users (id, email, username, password_hash) VALUES (?, ?, ?, ?)",
                    params: [Integer(input.id), Text(input.email), Text(input.username), Text(input.hash)],
                }),
                on_error,
            )
            .fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT INTO tokens (token, user_id) VALUES (?, ?)",
                params: [Text(input.token), Integer(input.id)],
            }))
            .json(200, Users.user_body({
                id: input.id,
                email: input.email,
                username: input.username,
                bio: NullableStr.Null,
                image: NullableStr.Null,
                token: input.token,
            }))
    }

    ## Coeffect: the login body verified against the stored hash — resolves
    ## to the authenticated user carrying a FRESH token, or rejects 422
    ## without leaking which of email/password was wrong.
    credentials! = |req| {
        body : { user : { email : Str, password : Str } }
        body = Req.json_body(req) ? |_| Api.unprocessable("invalid login body")
        db = Box.unbox(req.model).db
        result = Host.sqlite_exec!({
            db: db,
            sql: "SELECT id, email, username, bio, image, password_hash FROM users WHERE email = ?",
            params: [Text(body.user.email)],
        }) ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.unprocessable("email or password is invalid")
        stored = row.str("password_hash") ? |_| Api.error(500, "corrupt user row")
        if Host.password_verify!(body.user.password, stored) {
            token = Auth.fresh_token!(req)?
            user = Auth.decode_user(row, token) ? |_| Api.error(500, "corrupt user row")
            Ok(user)
        } else {
            Err(Api.unprocessable("email or password is invalid"))
        }
    }

    ## PURE handler: store the fresh token, answer the user envelope.
    login : Auth.User -> Res.Response
    login = |user| {
        Res.fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT INTO tokens (token, user_id) VALUES (?, ?)",
                params: [Text(user.token), Integer(user.id)],
            }))
            .json(200, Users.user_body(user))
    }

    ## PURE handler: the current user, straight from the auth coeffect.
    current : Auth.User -> Res.Response
    current = |user| Res.json(200, Users.user_body(user))

    ## Coeffect: the authenticated user plus the decoded partial update.
    ## Every field is optional (`Try(_, [Missing])` decodes absent fields);
    ## a new password is hashed here so the handler stays pure. The hash
    ## crosses as a SQL param: TEXT to replace, NULL to keep (COALESCE).
    update_input! = |req| {
        viewer = Auth.current_user!(req)?
        body : {
            user : {
                email : Try(Str, [Missing]),
                username : Try(Str, [Missing]),
                password : Try(Str, [Missing]),
                bio : Try(Str, [Missing]),
                image : Try(Str, [Missing]),
            },
        }
        body = Req.json_body(req) ? |_| Api.unprocessable("invalid update body")
        hash_param = match body.user.password {
            Ok(password) => Text(Host.password_hash!(password))
            Err(Missing) => Null
        }
        Ok({ current: viewer, changes: body.user, hash_param: hash_param })
    }

    ## A partial-update field: the sent value, or the current one when absent.
    or_keep : Try(Str, [Missing]), Str -> Str
    or_keep = |field, existing| match field {
        Ok(value) => value
        Err(Missing) => existing
    }

    ## PURE handler: merge changes over the current user, UPDATE in one
    ## Must (unique violations answer 422), echo the merged user.
    update = |input| {
        email = Users.or_keep(input.changes.email, input.current.email)
        username = Users.or_keep(input.changes.username, input.current.username)
        bio = match input.changes.bio {
            Ok(text) => NullableStr.Val(text)
            Err(Missing) => input.current.bio
        }
        image = match input.changes.image {
            Ok(text) => NullableStr.Val(text)
            Err(Missing) => input.current.image
        }
        on_error = |err| match err {
            UniqueViolation(_) => Api.reply(422, "username or email has already been taken")
            _ => Api.reply(500, "update failed")
        }
        Res.fx_err(
                DbWrite({
                    db: Db.db_path,
                    sql: "UPDATE users SET email = ?, username = ?, bio = ?, image = ?, password_hash = COALESCE(?, password_hash) WHERE id = ?",
                    params: [
                        Text(email),
                        Text(username),
                        Db.to_sql(bio),
                        Db.to_sql(image),
                        input.hash_param,
                        Integer(input.current.id),
                    ],
                }),
                on_error,
            )
            .json(200, Users.user_body({
                id: input.current.id,
                email: email,
                username: username,
                bio: bio,
                image: image,
                token: input.current.token,
            }))
    }
}

## user_body wraps the user in the RealWorld envelope with nullable fields
expect {
    body = Users.user_body({ id: 1, email: "j@x.com", username: "jake", bio: NullableStr.Null, image: NullableStr.Null, token: "tok" })
    Json.to_str(body) == "{\"user\":{\"bio\":null,\"email\":\"j@x.com\",\"image\":null,\"token\":\"tok\",\"username\":\"jake\"}}"
}
