## Per-request coeffect resolution with host-backed memoization, and the
## coeffect wrapper family (`args1..args5`) — the pure-handler successor to
## `Req.argsN`.
##
## `get!` resolves a coeffect at most once per REQUEST, keyed by the host-minted
## ctx id (`req.ctx`) plus an author-supplied `Str`. On a miss it runs the
## injector and stores a boxed THUNK capturing the result (the host can release
## an erased callable soundly at teardown; it cannot release an opaque value
## box). On a hit it retrieves the stored thunk and invokes it, which is a
## static-dispatch-known downcast back to `a` — memory-safe only because the
## key uniquely identifies the type within one request.
##
## `args1..args5` build the request handler for a PURE `->` handler that takes
## positional coeffects. Each coeffect type supplies an effectful
## `inject! : req => Try(a, resp)` that resolves it FROM the request (reading
## `req.ctx` to memoize through `get!`, so a non-idempotent coeffect like
## `Req.ReservedId` runs once even when several slots request it). The wrapper
## resolves each positional coeffect left to right, returns the first Err
## rejection as the response, else calls the pure handler and returns its
## `Response`. The wrapper is effectful (`=>`); the handler is pure (`->`) and
## may only CONSTRUCT effects — the host runs the returned effect list.
Ctx := [].{
    ## Resolve `key`'s coeffect for request `ctx_id`, running `inject!` only on
    ## the first call within that request.
    get! : U64, Str, ({} => a) => a
    get! = |ctx_id, key, inject!| {
        match Host.memo_get!(ctx_id, key) {
            Ok(boxed) => {
                thunk = Box.unbox(boxed)
                thunk({})
            }
            Err(Miss) => {
                value = inject!({})
                Host.memo_put!(ctx_id, key, Box.box(|{}| value))
                value
            }
        }
    }

    ## Wrap a 1-argument PURE handler into an effectful request handler. The
    ## argument type's `inject!` method resolves it from the request; an Err
    ## rejection is returned as the response. Coeffects resolve left to right;
    ## the first rejection wins.
    args1 : (a -> resp) -> (req => resp)
        where [a.inject! : req => Try(a, resp)]
    args1 = |handler| {
        A : a
        |req| match A.inject!(req) {
            Ok(x) => handler(x)
            Err(rejection) => rejection
        }
    }

    ## Wrap a 2-argument PURE handler into a request handler (see args1).
    args2 : (a, b -> resp) -> (req => resp)
        where [
            a.inject! : req => Try(a, resp),
            b.inject! : req => Try(b, resp),
        ]
    args2 = |handler| {
        A : a
        B : b
        |req| match A.inject!(req) {
            Err(rejection) => rejection
            Ok(x) => match B.inject!(req) {
                Err(rejection) => rejection
                Ok(y) => handler(x, y)
            }
        }
    }

    ## Wrap a 3-argument PURE handler into a request handler (see args1).
    args3 : (a, b, c -> resp) -> (req => resp)
        where [
            a.inject! : req => Try(a, resp),
            b.inject! : req => Try(b, resp),
            c.inject! : req => Try(c, resp),
        ]
    args3 = |handler| {
        A : a
        B : b
        C : c
        |req| match A.inject!(req) {
            Err(rejection) => rejection
            Ok(x) => match B.inject!(req) {
                Err(rejection) => rejection
                Ok(y) => match C.inject!(req) {
                    Err(rejection) => rejection
                    Ok(z) => handler(x, y, z)
                }
            }
        }
    }

    ## Wrap a 4-argument PURE handler into a request handler (see args1).
    args4 : (a, b, c, d -> resp) -> (req => resp)
        where [
            a.inject! : req => Try(a, resp),
            b.inject! : req => Try(b, resp),
            c.inject! : req => Try(c, resp),
            d.inject! : req => Try(d, resp),
        ]
    args4 = |handler| {
        A : a
        B : b
        C : c
        D : d
        |req| match A.inject!(req) {
            Err(rejection) => rejection
            Ok(x) => match B.inject!(req) {
                Err(rejection) => rejection
                Ok(y) => match C.inject!(req) {
                    Err(rejection) => rejection
                    Ok(z) => match D.inject!(req) {
                        Err(rejection) => rejection
                        Ok(w) => handler(x, y, z, w)
                    }
                }
            }
        }
    }

    ## Wrap a 5-argument PURE handler into a request handler (see args1).
    args5 : (a, b, c, d, e -> resp) -> (req => resp)
        where [
            a.inject! : req => Try(a, resp),
            b.inject! : req => Try(b, resp),
            c.inject! : req => Try(c, resp),
            d.inject! : req => Try(d, resp),
            e.inject! : req => Try(e, resp),
        ]
    args5 = |handler| {
        A : a
        B : b
        C : c
        D : d
        E : e
        |req| match A.inject!(req) {
            Err(rejection) => rejection
            Ok(x) => match B.inject!(req) {
                Err(rejection) => rejection
                Ok(y) => match C.inject!(req) {
                    Err(rejection) => rejection
                    Ok(z) => match D.inject!(req) {
                        Err(rejection) => rejection
                        Ok(w) => match E.inject!(req) {
                            Err(rejection) => rejection
                            Ok(v) => handler(x, y, z, w, v)
                        }
                    }
                }
            }
        }
    }
}

import Host
