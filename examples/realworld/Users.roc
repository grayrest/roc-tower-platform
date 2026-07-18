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
## Body decode + validation are their own coeffects (`registration!`,
## `credentials!`, `user_patch!`); routes bind them alongside the shared
## auth/id/token coeffects with `withN`, and the pure handlers only ever
## see input that passed.
Users := [].{
    ## What POST /api/users decodes.
    Registration : { username : Str, email : Str, password : Str }

    ## What POST /api/users/login decodes.
    Credentials : { email : Str, password : Str }

    ## What PUT /api/user decodes — every field optional;
    ## email/username/password are non-nullable (explicit null fails the
    ## decode), bio/image are tri-state (absent / null / value).
    UserPatch : {
        email : Try(Str, [Missing]),
        username : Try(Str, [Missing]),
        password : Try(Str, [Missing]),
        bio : Try(NullableStr, [Missing]),
        image : Try(NullableStr, [Missing]),
    }

    routes = |{ prefix, route }| [
        route("POST", "${prefix}/users", Users.register_h!),
        route("POST", "${prefix}/users/login", Users.login_h!),
        route("GET", "${prefix}/user", Users.current_h!),
        route("PUT", "${prefix}/user", Users.update_h!),
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

    ## Coeffect: the decoded, VALIDATED registration, password already
    ## hashed host-side (hashing is a READ — it must precede the pure
    ## handler).
    registration! = |req| {
        body : { user : Users.Registration }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid registration body")
        Ok({
            username: Api.nonblank("username", body.user.username)?,
            email: Api.nonblank("email", body.user.email)?,
            hash: Host.password_hash!(valid_password(body.user.password)?),
        })
    }

    ## `register` bound to its coeffects (validated body, id, token).
    register_h! = Cofx.bind3(Users.registration!, Req.reserved_id!, Auth.fresh_token!, Users.register)

    ## PURE handler: INSERT the user and their first token in one
    ## transaction; a duplicate email/username fails the Must and the
    ## err_handler answers 409 keyed by the colliding column.
    register : { username : Str, email : Str, hash : Str }, I64, Str -> Res.Response
    register = |reg, id, token| {
        on_error = |err| match err {
            UniqueViolation(detail) => Api.reply(409, unique_field(detail), "has already been taken")
            _ => Api.reply(500, "body", "registration failed")
        }
        Res.fx_err(
                DbWrite({
                    db: Db.db_path,
                    sql: "INSERT INTO users (id, email, username, password_hash) VALUES (?, ?, ?, ?)",
                    params: [Integer(id), Text(reg.email), Text(reg.username), Text(reg.hash)],
                }),
                on_error,
            )
            .fx(DbWrite({
                db: Db.db_path,
                sql: "INSERT INTO tokens (token, user_id) VALUES (?, ?)",
                params: [Text(token), Integer(id)],
            }))
            .json(201, Users.user_body({
                id: id,
                email: reg.email,
                username: reg.username,
                bio: NullableStr.Null,
                image: NullableStr.Null,
                token: token,
            }))
    }

    ## Coeffect: the login body verified against the stored hash — resolves
    ## to the authenticated user carrying a FRESH token (a domain unit, not
    ## an input bag: "verified credentials"). Blank fields are 422; wrong
    ## email or password is 401 credentials/invalid, without leaking which.
    credentials! = |req| {
        body : { user : Users.Credentials }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid login body")
        email = Api.nonblank("email", body.user.email)?
        password = Api.nonblank("password", body.user.password)?
        rows = Sql.query!(
            req.ctx,
            Db.of(req),
            "SELECT id, email, username, bio, image, password_hash FROM users WHERE email = ?",
            [Text(email)],
        )?
        row = Sql.first(rows) ? |_| Fail(401.U16, "credentials", "invalid")
        stored = row.str("password_hash") ? |_| Fail(500.U16, "body", "corrupt user row")
        if Host.password_verify!(password, stored) {
            token = Auth.fresh_token!(req)?
            user = Auth.decode_user(row, token) ? |_| Fail(500.U16, "body", "corrupt user row")
            Ok(user)
        } else {
            Err(Fail(401.U16, "credentials", "invalid"))
        }
    }

    ## `login` bound to the verified-credentials coeffect.
    login_h! = Cofx.bind1(Users.credentials!, Users.login)

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

    ## `current` bound to the auth coeffect.
    current_h! = Cofx.bind1(Auth.current_user!, Users.current)

    ## PURE handler: the current user, straight from the auth coeffect.
    current : Auth.User -> Res.Response
    current = |user| Res.json(200, Users.user_body(user))

    ## Coeffect: the decoded, VALIDATED partial update, a new password
    ## already hashed. The hash crosses as a SQL param — TEXT to replace,
    ## NULL to keep (COALESCE).
    user_patch! = |req| {
        body : { user : Users.UserPatch }
        body = Req.json_body!(req) ? |_| Fail(422.U16, "body", "invalid update body")
        Ok({
            email: Api.nonblank_opt("email", body.user.email)?,
            username: Api.nonblank_opt("username", body.user.username)?,
            hash_param: match body.user.password {
                Ok(password) => Text(Host.password_hash!(valid_password(password)?))
                Err(Missing) => Null
            },
            bio: body.user.bio,
            image: body.user.image,
        })
    }

    ## `update` bound to its coeffects (auth, validated patch).
    update_h! = Cofx.bind2(Auth.current_user!, Users.user_patch!, Users.update)

    ## PURE handler: merge the patch over the current user, UPDATE in one
    ## Must (duplicates answer 409 like registration), echo the merged user.
    update = |viewer, patch| {
        email = or_keep(patch.email, viewer.email)
        username = or_keep(patch.username, viewer.username)
        bio = merge_nullable(patch.bio, viewer.bio)
        image = merge_nullable(patch.image, viewer.image)
        on_error = |err| match err {
            UniqueViolation(detail) => Api.reply(409, unique_field(detail), "has already been taken")
            _ => Api.reply(500, "body", "update failed")
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
                        patch.hash_param,
                        Integer(viewer.id),
                    ],
                }),
                on_error,
            )
            .json(200, Users.user_body({
                id: viewer.id,
                email: email,
                username: username,
                bio: bio,
                image: image,
                token: viewer.token,
            }))
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: NIST 800-63B minimum (the spec's stated policy): at least 8
## chars; blankness is reported first, as its own message.
valid_password : Str -> Try(Str, [Fail(U16, Str, Str), ..])
valid_password = |password| {
    checked = Api.nonblank("password", password)?
    if checked.count_utf8_bytes() < 8 {
        Err(Fail(422.U16, "password", "is too short (minimum is 8 characters)"))
    } else {
        Ok(checked)
    }
}

## Internal: which column a users-table UNIQUE violation names, as the
## error key.
unique_field : Str -> Str
unique_field = |detail| if detail.contains("users.email") "email" else "username"

## Internal: a partial-update field — the sent value, or the current one.
or_keep : Try(Str, [Missing]), Str -> Str
or_keep = |field, existing| match field {
    Ok(value) => value
    Err(Missing) => existing
}

## Internal: a nullable PATCH field, merged and normalized: absent keeps
## the current value; an empty string normalizes to null (the spec's
## bio/image rule); explicit null and real values pass through.
merge_nullable : Try(NullableStr, [Missing]), NullableStr -> NullableStr
merge_nullable = |field, existing| match field {
    Err(Missing) => existing
    Ok(Val(text)) => if text.trim().is_empty() NullableStr.Null else NullableStr.Val(text)
    Ok(Null) => NullableStr.Null
}

## user_body wraps the user in the RealWorld envelope with nullable fields
expect {
    body = Users.user_body({ id: 1, email: "j@x.com", username: "jake", bio: NullableStr.Null, image: NullableStr.Null, token: "tok" })
    Json.to_str(body) == "{\"user\":{\"bio\":null,\"email\":\"j@x.com\",\"image\":null,\"token\":\"tok\",\"username\":\"jake\"}}"
}

## the password policy floors at 8 chars with the spec's message
expect valid_password("short7c") == Err(Fail(422.U16, "password", "is too short (minimum is 8 characters)"))

## an 8-char simple password is accepted (no composition rules)
expect valid_password("bonjour1") == Ok("bonjour1")

## a blank password reports can't-be-blank, not too-short
expect valid_password("") == Err(Fail(422.U16, "password", "can't be blank"))

## unique_field attributes the violated column
expect unique_field("UNIQUE constraint failed: users.email") == "email"

## merge_nullable: absent keeps, empty string normalizes to null
expect {
    kept = merge_nullable(Err(Missing), NullableStr.Val("old"))
    cleared = merge_nullable(Ok(NullableStr.Val("")), NullableStr.Val("old"))
    kept == NullableStr.Val("old") and cleared == NullableStr.Null
}
