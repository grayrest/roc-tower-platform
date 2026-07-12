import pf.Host
import pf.Req
import pf.Cofx
import pf.Sql
import pf.NullableStr
import Api

## Token auth as coeffects. Tokens are opaque 32-hex-char strings minted from
## `Host.random_bytes!` and stored in the `tokens` table; the auth lookup
## (header parse + DB join) is memoized per request, so `current_user!` and
## `optional_user!` in any combination cost ONE query.
Auth := [].{
    ## The authenticated user, carrying the token that authenticated this
    ## request (user responses echo it back).
    User : {
        id : I64,
        email : Str,
        username : Str,
        bio : NullableStr,
        image : NullableStr,
        token : Str,
    }

    ## The resolved auth state of a request.
    Viewer : [Anon, Found(Auth.User)]

    ## Pure: the token from an `Authorization: Token <hex>` header.
    token_of : { headers : List({ name : Str, value : Str }), .. } -> Try(Str, [NoToken])
    token_of = |req| {
        value = Req.get_header(req, "authorization") ? |_| NoToken
        match value.split_on(" ") {
            ["Token", token] => Ok(token)
            _ => Err(NoToken)
        }
    }

    ## Memoized auth state: the header parse + token join runs at most once
    ## per request, whichever of the coeffects below ask for it.
    lookup! = |req| Cofx.get!(req.ctx, "auth.viewer", |{}| Auth.fetch!(req))

    ## Internal: the memo-miss path. Any failure along the way (no header,
    ## malformed header, unknown token) is simply an anonymous request.
    fetch! = |req| {
        match Auth.token_of(req) {
            Err(NoToken) => Anon
            Ok(token) => {
                db = Box.unbox(req.model).db
                sql = "SELECT u.id, u.email, u.username, u.bio, u.image FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.token = ?"
                match Host.sqlite_exec!({ db: db, sql: sql, params: [Text(token)] }) {
                    Err(_) => Anon
                    Ok(result) => match Sql.first(result) {
                        Err(_) => Anon
                        Ok(row) => match Auth.decode_user(row, token) {
                            Ok(user) => Found(user)
                            Err(_) => Anon
                        }
                    }
                }
            }
        }
    }

    ## Pure: decode the user columns (id, email, username, bio, image),
    ## attaching the token that proved them. Shared with the login flow.
    decode_user : Sql.Row, Str -> Try(Auth.User, Sql.Err)
    decode_user = |row, token| {
        id = row.i64("id")?
        email = row.str("email")?
        username = row.str("username")?
        bio = row.nullable_str("bio")?
        image = row.nullable_str("image")?
        Ok({ id: id, email: email, username: username, bio: bio, image: image, token: token })
    }

    ## Coeffect: the viewer, required — rejects 401 when anonymous.
    current_user! = |req| match Auth.lookup!(req) {
        Found(user) => Ok(user)
        Anon => Err(Api.unauthorized())
    }

    ## Coeffect: the viewer, optional — anonymous requests resolve to `Anon`
    ## instead of rejecting (list/get endpoints vary output by viewer).
    optional_user! = |req| Ok(Auth.lookup!(req))

    ## The id SQL flag subqueries compare against: 0 for anonymous (ids
    ## start at 1, so it never matches).
    viewer_id : Auth.Viewer -> I64
    viewer_id = |viewer| match viewer {
        Found(user) => user.id
        Anon => 0
    }

    ## Coeffect: a freshly minted token, memoized per request (register and
    ## login each store exactly one).
    fresh_token! = |req| Ok(Cofx.get!(req.ctx, "auth.fresh_token", |{}| Auth.mint!()))

    ## Internal: 16 random bytes as 32 hex chars.
    mint! = || Auth.hex(Host.random_bytes!(16))

    ## Pure: lowercase hex encoding.
    hex : List(U8) -> Str
    hex = |bytes| {
        digits = bytes.fold([], |acc, byte| {
            acc.append(Auth.hex_digit(byte // 16)).append(Auth.hex_digit(byte % 16))
        })
        Str.from_utf8_lossy(digits)
    }

    ## Internal: one hex digit's ASCII byte.
    hex_digit : U8 -> U8
    hex_digit = |n| if n < 10 n + 48 else n + 87
}

## token_of extracts the token from a well-formed Authorization header
expect Auth.token_of({ headers: [{ name: "Authorization", value: "Token abc123" }] }) == Ok("abc123")

## token_of rejects a Bearer-style header (RealWorld uses the Token scheme)
expect Auth.token_of({ headers: [{ name: "Authorization", value: "Bearer abc123" }] }) == Err(NoToken)

## token_of rejects a request with no Authorization header
expect Auth.token_of({ headers: [] }) == Err(NoToken)

## hex encodes bytes as lowercase pairs
expect Auth.hex([0, 15, 255]) == "000fff"

## viewer_id is 0 for anonymous requests
expect Auth.viewer_id(Anon) == 0

## decode_user attaches the presented token to the decoded row
expect {
    row = Sql.Row.{
        columns: ["id", "email", "username", "bio", "image"],
        cells: [Integer(1), Text("j@x.com"), Text("jake"), Null, Null],
    }
    match Auth.decode_user(row, "tok") {
        Ok(user) => user.username == "jake" and user.token == "tok"
        Err(_) => False
    }
}
