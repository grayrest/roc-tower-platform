## Connect protocol (https://connectrpc.com/docs/protocol/) helpers for the
## UNARY + JSON subset. That subset is the entire server-side wire contract
## here: a success is HTTP `200` with the response message as the JSON body; an
## error is the code's mapped HTTP status with a `{ code, message }` JSON body.
## No length-prefix framing, no HTTP trailers, no HTTP/2 — those only appear in
## the streaming and binary variants, which this demo deliberately omits.
Connect := [].{
    ## A Connect error code (the subset this demo raises). The full set and the
    ## exact status mapping are in the protocol reference linked above.
    Code : [
        InvalidArgument,
        NotFound,
        AlreadyExists,
        Unauthenticated,
        PermissionDenied,
        FailedPrecondition,
        ResourceExhausted,
        Unimplemented,
        Unavailable,
        Internal,
    ]

    # NOTE: no `Connect.ok` wrapper — a generic pass-through method forwarding
    # to `Res.json` still crashes lowering on the current compiler (postcheck
    # invariant: erroneous checked type reached Monotype instantiation). Handlers
    # call `Res.json(200, msg)` directly.

    ## The app-side domain error: a Connect code paired with a message.
    ## Coeffects raise these with `?` — `? |_| NotFound("...")` for a literal,
    ## or a bare constructor like `? Internal` to wrap the underlying `Str`
    ## error — and the `route` wrapper renders them via main.roc's `render`.
    Error : [
        InvalidArgument(Str),
        NotFound(Str),
        Internal(Str),
    ]

    ## A Connect error: the code's mapped HTTP status and a `{ code, message }`
    ## JSON body — the shape a Connect client decodes into a typed error.
    ## Unannotated (like realworld's `Api.field_error`): naming the `Res.Response`
    ## return type still crashes lowering across the module boundary (same
    ## postcheck invariant as the `Connect.ok` case), so the empty `effects: []`
    ## is left to unify.
    error = |code, message| {
        # Pin `message` to Str locally (no signature), so `Json.to_str` can
        # resolve an encoder for the `{ code, message }` record.
        text : Str
        text = message
        {
            status: status_of(code),
            headers: [{ name: "content-type", value: "application/json" }],
            body: Json.to_str({ code: string_of(code), message: text }).to_utf8(),
            effects: [],
        }
    }
}

import pf.Res

# --- module-private: the code <-> HTTP-status <-> wire-string mapping ---

status_of : Connect.Code -> U16
status_of = |code| match code {
    InvalidArgument => 400
    NotFound => 404
    AlreadyExists => 409
    Unauthenticated => 401
    PermissionDenied => 403
    FailedPrecondition => 400
    ResourceExhausted => 429
    Unimplemented => 501
    Unavailable => 503
    Internal => 500
}

string_of : Connect.Code -> Str
string_of = |code| match code {
    InvalidArgument => "invalid_argument"
    NotFound => "not_found"
    AlreadyExists => "already_exists"
    Unauthenticated => "unauthenticated"
    PermissionDenied => "permission_denied"
    FailedPrecondition => "failed_precondition"
    ResourceExhausted => "resource_exhausted"
    Unimplemented => "unimplemented"
    Unavailable => "unavailable"
    Internal => "internal"
}

## not_found maps to HTTP 404
expect status_of(NotFound) == 404

## invalid_argument maps to HTTP 400
expect status_of(InvalidArgument) == 400

## codes serialize to their lower_snake_case wire spelling
expect string_of(InvalidArgument) == "invalid_argument"

# NOTE: the code->status and code->wire-string mappings are unit-tested above
# via the module-private `status_of`/`string_of`. The `Connect.ok`/`error`
# WRAPPERS aren't expect-tested: evaluating a call to an example-module nominal
# method that returns the platform's `Res.Response` inside an `expect` segfaults
# the compiler (repro + details in notes/2026-07-16-compiler-expect-nominal-
# response-segfault.md). `roc check`/`roc build` are unaffected; the wrappers'
# behavior is covered end-to-end by bench/connect-check.sh.
