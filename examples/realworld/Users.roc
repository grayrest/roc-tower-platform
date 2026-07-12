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
## Validation lives in the coeffects (a rejection IS the response), so the
## pure handlers only ever see input that passed.
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

    ## NIST 800-63B minimum (the spec's stated policy): at least 8 chars.
    ## Blankness is reported first, as its own message.
    valid_password : Str -> Try(Str, Res.Response)
    valid_password = |password| {
        checked = Api.nonblank("password", password)?
        if checked.count_utf8_bytes() < 8 {
            Err(Api.field_error(422, "password", "is too short (minimum is 8 characters)"))
        } else {
            Ok(checked)
        }
    }

    ## Which column a users-table UNIQUE violation names, as the error key.
    unique_field : Str -> Str
    unique_field = |detail| if detail.contains("users.email") "email" else "username"

    ## Coeffect: everything registration needs — the decoded and VALIDATED
    ## body, a reserved user id, a fresh token, and the password hashed
    ## host-side (a READ: hashing must happen before the pure handler).
    registration! = |req| {
        body : { user : { username : Str, email : Str, password : Str } }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid registration body")
        username = Api.nonblank("username", body.user.username)?
        email = Api.nonblank("email", body.user.email)?
        password = Users.valid_password(body.user.password)?
        id = Req.reserved_id!(req)?
        token = Auth.fresh_token!(req)?
        hash = Host.password_hash!(password)
        Ok({ id: id, email: email, username: username, hash: hash, token: token })
    }

    ## PURE handler: INSERT the user and their first token in one
    ## transaction; a duplicate email/username fails the Must and the
    ## err_handler answers 409 keyed by the colliding column.
    register : { id : I64, email : Str, username : Str, hash : Str, token : Str } -> Res.Response
    register = |input| {
        on_error = |err| match err {
            UniqueViolation(detail) => Api.reply(409, Users.unique_field(detail), "has already been taken")
            _ => Api.reply(500, "body", "registration failed")
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
            .json(201, Users.user_body({
                id: input.id,
                email: input.email,
                username: input.username,
                bio: NullableStr.Null,
                image: NullableStr.Null,
                token: input.token,
            }))
    }

    ## Coeffect: the login body verified against the stored hash — resolves
    ## to the authenticated user carrying a FRESH token. Blank fields are
    ## 422; wrong email or password is 401 credentials/invalid, without
    ## leaking which.
    credentials! = |req| {
        body : { user : { email : Str, password : Str } }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid login body")
        email = Api.nonblank("email", body.user.email)?
        password = Api.nonblank("password", body.user.password)?
        db = Box.unbox(req.model).db
        result = Host.sqlite_exec!({
            db: db,
            sql: "SELECT id, email, username, bio, image, password_hash FROM users WHERE email = ?",
            params: [Text(email)],
        }) ? |message| Api.error(500, "db error: ${message}")
        row = Sql.first(result) ? |_| Api.invalid_credentials()
        stored = row.str("password_hash") ? |_| Api.error(500, "corrupt user row")
        if Host.password_verify!(password, stored) {
            token = Auth.fresh_token!(req)?
            user = Auth.decode_user(row, token) ? |_| Api.error(500, "corrupt user row")
            Ok(user)
        } else {
            Err(Api.invalid_credentials())
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

    ## A nullable PATCH field, merged and normalized: absent keeps the
    ## current value; an empty string normalizes to null (the spec's
    ## bio/image rule); explicit null and real values pass through.
    merge_nullable : Try(NullableStr, [Missing]), NullableStr -> NullableStr
    merge_nullable = |field, existing| match field {
        Err(Missing) => existing
        Ok(Val(text)) => if text.trim().is_empty() NullableStr.Null else NullableStr.Val(text)
        Ok(Null) => NullableStr.Null
    }

    ## Coeffect: the authenticated user merged with the decoded, VALIDATED
    ## partial update. email/username/password are optional but non-nullable
    ## (explicit null fails the decode → 422); bio/image are tri-state
    ## (`Try(NullableStr, [Missing])`): absent keeps, "" and null clear.
    ## A new password is hashed here so the handler stays pure; the hash
    ## crosses as a SQL param — TEXT to replace, NULL to keep (COALESCE).
    update_input! = |req| {
        viewer = Auth.current_user!(req)?
        body : {
            user : {
                email : Try(Str, [Missing]),
                username : Try(Str, [Missing]),
                password : Try(Str, [Missing]),
                bio : Try(NullableStr, [Missing]),
                image : Try(NullableStr, [Missing]),
            },
        }
        body = Req.json_body(req) ? |_| Api.error(422, "invalid update body")
        email = match body.user.email {
            Ok(value) => Api.nonblank("email", value)?
            Err(Missing) => viewer.email
        }
        username = match body.user.username {
            Ok(value) => Api.nonblank("username", value)?
            Err(Missing) => viewer.username
        }
        hash_param = match body.user.password {
            Ok(password) => Text(Host.password_hash!(Users.valid_password(password)?))
            Err(Missing) => Null
        }
        Ok({
            current: viewer,
            email: email,
            username: username,
            bio: Users.merge_nullable(body.user.bio, viewer.bio),
            image: Users.merge_nullable(body.user.image, viewer.image),
            hash_param: hash_param,
        })
    }

    ## PURE handler: UPDATE the merged user in one Must (duplicates answer
    ## 409 like registration), echo the merged user.
    update = |input| {
        on_error = |err| match err {
            UniqueViolation(detail) => Api.reply(409, Users.unique_field(detail), "has already been taken")
            _ => Api.reply(500, "body", "update failed")
        }
        Res.fx_err(
                DbWrite({
                    db: Db.db_path,
                    sql: "UPDATE users SET email = ?, username = ?, bio = ?, image = ?, password_hash = COALESCE(?, password_hash) WHERE id = ?",
                    params: [
                        Text(input.email),
                        Text(input.username),
                        Db.to_sql(input.bio),
                        Db.to_sql(input.image),
                        input.hash_param,
                        Integer(input.current.id),
                    ],
                }),
                on_error,
            )
            .json(200, Users.user_body({
                id: input.current.id,
                email: input.email,
                username: input.username,
                bio: input.bio,
                image: input.image,
                token: input.current.token,
            }))
    }
}

## user_body wraps the user in the RealWorld envelope with nullable fields
expect {
    body = Users.user_body({ id: 1, email: "j@x.com", username: "jake", bio: NullableStr.Null, image: NullableStr.Null, token: "tok" })
    Json.to_str(body) == "{\"user\":{\"bio\":null,\"email\":\"j@x.com\",\"image\":null,\"token\":\"tok\",\"username\":\"jake\"}}"
}

## the password policy floors at 8 chars with the spec's message
expect {
    match Users.valid_password("short7c") {
        Ok(_) => False
        Err(response) => response.status == 422
    }
}

## an 8-char simple password is accepted (no composition rules)
expect Users.valid_password("bonjour1") == Ok("bonjour1")

## a blank password reports can't-be-blank, not too-short
expect {
    match Users.valid_password("") {
        Ok(_) => False
        Err(response) => Str.from_utf8_lossy(response.body).contains("can't be blank")
    }
}

## unique_field attributes the violated column
expect Users.unique_field("UNIQUE constraint failed: users.email") == "email"

## merge_nullable: absent keeps, empty string normalizes to null
expect {
    kept = Users.merge_nullable(Err(Missing), NullableStr.Val("old"))
    cleared = Users.merge_nullable(Ok(NullableStr.Val("")), NullableStr.Val("old"))
    kept == NullableStr.Val("old") and cleared == NullableStr.Null
}
