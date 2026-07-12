## Coeffects as VALUES, plus per-request memoization.
##
## A coeffect is a plain resolver function: `req => Try(a, response)`. It
## either produces the value a pure handler wants, or rejects with the
## response to send instead. Because a resolver is an ordinary value:
##
##   - PARAMETERIZING one is partial application — `Req.param_named("id")`,
##     `paginated(50)`, `require_role("admin")` are functions returning
##     resolvers.
##   - COMPOSING them is ordinary function composition — `Ctx.map` transforms
##     a resolved value; `Ctx.and_then` builds a resolver whose work depends
##     on another resolver's output (session -> user -> permissions).
##   - REUSING one across routes is just naming it.
##
## `Ctx.with1` … `Ctx.with5` bind resolvers to a PURE handler's positional
## arguments: each resolver runs left to right (this is where reads happen —
## parse a param, load a row, reserve an id), the first `Err` becomes the
## response and the handler never runs, otherwise the handler is called with
## the RAW resolved values. The wrapper is effectful (`=>`); the handler is
## pure (`->`) and may only CONSTRUCT effects — the host runs the returned
## effect list.
##
## Memoization is OPT-IN, for resolvers that must run at most once per request
## (non-idempotent reads like `Req.reserved_id`) or that several resolvers
## share (a session both `current_user` and `csrf_check` need): route the work
## through `Ctx.get!` with a key that INCLUDES every parameter the resolver
## was built with (`"user:${id}"`), so differently-parameterized instances
## never alias.
Ctx := [].{
    ## Resolve `key`'s coeffect for request `ctx_id`, running `inject!` only on
    ## the first call within that request. The host stores a boxed THUNK
    ## capturing the result (it can release an erased callable soundly at
    ## teardown; it cannot release an opaque value box); a hit invokes the
    ## stored thunk, a statically-known downcast back to `a` — memory-safe only
    ## because the key uniquely identifies the type within one request.
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

    ## Transform a resolver's resolved value with a pure function, leaving
    ## rejections untouched. Shape (left unannotated so purity stays inferred:
    ## a pure resolver in gives a pure resolver out):
    ##
    ##     map : (req => Try(a, resp)), (a -> b) -> (req => Try(b, resp))
    ##     user_name = Ctx.map(current_user, |user| user.name)
    map = |resolver!, transform| |req| match resolver!(req) {
        Ok(value) => Ok(transform(value))
        Err(rejection) => Err(rejection)
    }

    ## Build a resolver that depends on another resolver's output. `next!`
    ## receives the resolved value AND the request, so a dependent read can
    ## reach the model, other params, or `Ctx.get!`. Shape (unannotated, like
    ## `map`):
    ##
    ##     and_then : (req => Try(a, resp)), (a, req => Try(b, resp)) -> (req => Try(b, resp))
    ##     fetched_user = Ctx.and_then(Req.param_named("id"), |id, req| load!(id, req))
    and_then = |resolver!, next!| |req| match resolver!(req) {
        Ok(value) => next!(value, req)
        Err(rejection) => Err(rejection)
    }

    ## Wrap a 0-coeffect PURE handler (one that reads nothing from the request,
    ## e.g. a handler that only emits effects) into an effectful request handler.
    with0 : (() -> resp) -> (req => resp)
    with0 = |handler| |_req| handler()

    ## Bind one resolver to a 1-argument PURE handler. The resolver runs first;
    ## an Err rejection is returned as the response, otherwise the handler
    ## receives the raw resolved value.
    with1 : (req => Try(a, resp)), (a -> resp) -> (req => resp)
    with1 = |resolve_a!, handler|
        |req| match resolve_a!(req) {
            Ok(a) => handler(a)
            Err(rejection) => rejection
        }

    ## Bind two resolvers to a 2-argument PURE handler (see with1). Resolvers
    ## run left to right; the first rejection wins.
    with2 : (req => Try(a, resp)), (req => Try(b, resp)), (a, b -> resp) -> (req => resp)
    with2 = |resolve_a!, resolve_b!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => rejection
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => rejection
                Ok(b) => handler(a, b)
            }
        }

    ## Bind three resolvers to a 3-argument PURE handler (see with1).
    with3 : (req => Try(a, resp)), (req => Try(b, resp)), (req => Try(c, resp)), (a, b, c -> resp) -> (req => resp)
    with3 = |resolve_a!, resolve_b!, resolve_c!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => rejection
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => rejection
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => rejection
                    Ok(c) => handler(a, b, c)
                }
            }
        }

    ## Bind four resolvers to a 4-argument PURE handler (see with1).
    with4 : (req => Try(a, resp)), (req => Try(b, resp)), (req => Try(c, resp)), (req => Try(d, resp)), (a, b, c, d -> resp) -> (req => resp)
    with4 = |resolve_a!, resolve_b!, resolve_c!, resolve_d!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => rejection
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => rejection
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => rejection
                    Ok(c) => match resolve_d!(req) {
                        Err(rejection) => rejection
                        Ok(d) => handler(a, b, c, d)
                    }
                }
            }
        }

    ## Bind five resolvers to a 5-argument PURE handler (see with1).
    with5 : (req => Try(a, resp)), (req => Try(b, resp)), (req => Try(c, resp)), (req => Try(d, resp)), (req => Try(e, resp)), (a, b, c, d, e -> resp) -> (req => resp)
    with5 = |resolve_a!, resolve_b!, resolve_c!, resolve_d!, resolve_e!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => rejection
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => rejection
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => rejection
                    Ok(c) => match resolve_d!(req) {
                        Err(rejection) => rejection
                        Ok(d) => match resolve_e!(req) {
                            Err(rejection) => rejection
                            Ok(e) => handler(a, b, c, d, e)
                        }
                    }
                }
            }
        }
}

import Host

# --- combinators (pure resolvers are callable in expects) ---

expect {
    resolver = Ctx.map(|req| Ok(req.n), |n| n + 1)
    resolver({ n: 41 }) == Ok(42.U64)
}

expect {
    resolver = Ctx.map(|_req| Err("nope"), |n| n + 1.U64)
    resolver({}) == Err("nope")
}

expect {
    resolver = Ctx.and_then(|req| Ok(req.n), |n, req| Ok(n + req.m))
    resolver({ n: 40, m: 2 }) == Ok(42.U64)
}

expect {
    resolver = Ctx.and_then(|_req| Ok(1.U64), |_n, _req| Err("second failed"))
    resolver({}) == Err("second failed")
}

expect {
    resolver = Ctx.and_then(|_req| Err("first failed"), |n, _req| Ok(n + 1.U64))
    resolver({}) == Err("first failed")
}
