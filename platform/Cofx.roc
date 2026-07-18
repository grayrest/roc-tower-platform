## Coeffects: the whole "in" side of a pure handler — resolvers, their
## combinators, the handler binders, and the per-request memo primitive.
##
## A coeffect is a PLAIN FUNCTION — `resolver : req => Try(a, resp)` — that
## either produces the value a pure handler wants, or rejects with the
## response to send instead. Because a resolver is an ordinary value:
##
##   - PARAMETERIZING one is partial application — `Req.param("id")`,
##     `paginated(50)`, `require_role("admin")` are functions returning
##     resolvers.
##   - PIPELINING uses the arrow-call operator, which feeds the left value in
##     as the first argument (`x->f(y)` is `f(x, y)`):
##
##         user_from!("id") = Req.param("id")
##             ->Cofx.and_then(|id, req| load_user!(id, req))
##             ->Cofx.map(|user| user.name)
##
##   - COMBINING independent resolvers into one NAMED block uses the record
##     builder, which desugars through `Cofx.map2` (the fields hold bare
##     resolver functions; the result is a bare resolver function):
##
##         inputs = {
##             id: Req.reserved_id!,
##             body: Req.json_body,
##         }.Cofx
##         # : req => Try({ id : I64, body : ... }, resp) — fields resolve in
##         # order, first rejection wins. Requires at least two fields.
##
## `Cofx.bind1` … `Cofx.bind5` bind resolvers to a PURE handler's positional
## arguments: each runs left to right (this is where reads happen — parse a
## param, load a row, reserve an id); the first `Err` short-circuits with the
## DOMAIN ERROR (the route's render turns it into a response — `Req.route`),
## otherwise the handler is called with the RAW resolved values and its
## response is the `Ok`. The binder is effectful (`=>`); the handler is pure
## (`->`) and may only CONSTRUCT effects — the host runs the returned effect
## list.
##
## Memoization is OPT-IN, for resolvers that must run at most once per request
## (non-idempotent reads like `Req.reserved_id!`) or that several resolvers
## share (a session both `current_user` and `csrf_check` need): route the work
## through `Cofx.get!` with a key that INCLUDES every parameter the resolver
## was built with (`"user:${id}"`), so differently-parameterized instances
## never alias.
##
## The combinators are left unannotated so purity stays inferred: composing
## pure resolvers yields a pure (expect-testable) resolver; effectful
## resolvers (named with `!`) yield effectful ones. Shapes:
##
##     map        : (req => Try(a, resp)), (a -> b) -> (req => Try(b, resp))
##     map_reject : (req => Try(a, resp)), (resp -> resp2) -> (req => Try(a, resp2))
##     resolve  : (req => Try(a, resp)), (a => Try(b, resp)) -> (req => Try(b, resp))
##     and_then : (req => Try(a, resp)), (a, req => Try(b, resp)) -> (req => Try(b, resp))
##     map2     : (req => Try(a, resp)), (req => Try(b, resp)), (a, b -> c) -> (req => Try(c, resp))
Cofx := [].{
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

    ## Transform the resolved value with a pure function; rejections pass
    ## through untouched.
    map = |resolver!, transform| |req| match resolver!(req) {
        Ok(value) => Ok(transform(value))
        Err(rejection) => Err(rejection)
    }

    ## Transform the REJECTION response, leaving resolved values untouched —
    ## rebrand a built-in rejection (e.g. `Req.json_body`'s 400 text) into the
    ## app's error shape without re-implementing the resolver.
    map_reject = |resolver!, transform| |req| match resolver!(req) {
        Ok(value) => Ok(value)
        Err(rejection) => Err(transform(rejection))
    }

    ## Chain a dependent step on the resolved value alone. Use `and_then`
    ## when the step also needs the request.
    resolve = |resolver!, next!| |req| match resolver!(req) {
        Ok(value) => next!(value)
        Err(rejection) => Err(rejection)
    }

    ## Chain a dependent step that receives the resolved value AND the
    ## request, so it can reach the model, other params, or `Cofx.get!` for
    ## memoization.
    and_then = |resolver!, next!| |req| match resolver!(req) {
        Ok(value) => next!(value, req)
        Err(rejection) => Err(rejection)
    }

    ## Combine two independent resolvers (in order, first rejection wins).
    ## This is the record-builder hook: `{ a: ra, b: rb }.Cofx` desugars into
    ## nested `map2` calls.
    map2 = |resolver_a!, resolver_b!, combine| |req| match resolver_a!(req) {
        Err(rejection) => Err(rejection)
        Ok(a_value) => match resolver_b!(req) {
            Err(rejection) => Err(rejection)
            Ok(b_value) => Ok(combine(a_value, b_value))
        }
    }

    ## --- Binders -------------------------------------------------------------
    ##
    ## `bindN` binds resolvers to a PURE handler's positional arguments:
    ## resolvers reject with an error VALUE (`req => Try(a, err)`), and the
    ## composed result is the fallible route-handler shape
    ## `req => Try(resp, err)` that `Req.route(method, path, handler, render)`
    ## renders. Because `err` is inferred as an open tag union, resolvers with
    ## different error tags merge, and a resolver that never rejects (e.g.
    ## `Req.reserved_id!`) stays polymorphic and binds bare:
    ##
    ##     show_h! = Cofx.bind2(book!, Req.reserved_id!, show)
    ##     route("POST", "/x", show_h!)   # route = Req.route(..., my_render)
    ##
    ## The first rejection wins and the pure handler never runs; otherwise the
    ## handler's response is the `Ok`. Left as straight code (no combinator
    ## composition), so inference flows through unimpeded.

    ## Bind one domain-fallible resolver to a 1-argument PURE handler.
    bind1 = |resolve_a!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => Err(rejection)
            Ok(a) => Ok(handler(a))
        }

    ## Bind two domain-fallible resolvers to a 2-argument PURE handler
    ## (resolvers run left to right; the first rejection wins).
    bind2 = |resolve_a!, resolve_b!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => Err(rejection)
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => Err(rejection)
                Ok(b) => Ok(handler(a, b))
            }
        }

    ## Bind three domain-fallible resolvers to a 3-argument PURE handler.
    bind3 = |resolve_a!, resolve_b!, resolve_c!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => Err(rejection)
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => Err(rejection)
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => Err(rejection)
                    Ok(c) => Ok(handler(a, b, c))
                }
            }
        }

    ## Bind four domain-fallible resolvers to a 4-argument PURE handler.
    bind4 = |resolve_a!, resolve_b!, resolve_c!, resolve_d!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => Err(rejection)
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => Err(rejection)
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => Err(rejection)
                    Ok(c) => match resolve_d!(req) {
                        Err(rejection) => Err(rejection)
                        Ok(d) => Ok(handler(a, b, c, d))
                    }
                }
            }
        }

    ## Bind five domain-fallible resolvers to a 5-argument PURE handler.
    bind5 = |resolve_a!, resolve_b!, resolve_c!, resolve_d!, resolve_e!, handler|
        |req| match resolve_a!(req) {
            Err(rejection) => Err(rejection)
            Ok(a) => match resolve_b!(req) {
                Err(rejection) => Err(rejection)
                Ok(b) => match resolve_c!(req) {
                    Err(rejection) => Err(rejection)
                    Ok(c) => match resolve_d!(req) {
                        Err(rejection) => Err(rejection)
                        Ok(d) => match resolve_e!(req) {
                            Err(rejection) => Err(rejection)
                            Ok(e) => Ok(handler(a, b, c, d, e))
                        }
                    }
                }
            }
        }
}

import Host

# --- combinators (pure resolvers are callable in expects) ---

expect {
    resolver = Cofx.map(|req| Ok(req.n), |n| n + 1)
    resolver({ n: 41 }) == Ok(42.U64)
}

expect {
    resolver = Cofx.map(|_req| Err("nope"), |n| n + 1.U64)
    resolver({}) == Err("nope")
}

## map_reject leaves resolved values untouched
expect {
    resolver = Cofx.map_reject(|req| Ok(req.n), |message| "wrapped: ${message}")
    resolver({ n: 42 }) == Ok(42.U64)
}

## map_reject transforms the rejection
expect {
    resolver = Cofx.map_reject(|_req| Err("nope"), |message| "wrapped: ${message}")
    resolver({}) == Err("wrapped: nope")
}

expect {
    resolver = Cofx.resolve(|req| Ok(req.n), |n| if n > 1 Ok(n) else Err("too small"))
    resolver({ n: 42 }) == Ok(42.U64)
}

expect {
    resolver = Cofx.resolve(|req| Ok(req.n), |n| if n > 100 Ok(n) else Err("too small"))
    resolver({ n: 42.U64 }) == Err("too small")
}

expect {
    resolver = Cofx.resolve(|_req| Err("first failed"), |n| Ok(n + 1.U64))
    resolver({}) == Err("first failed")
}

expect {
    resolver = Cofx.and_then(|req| Ok(req.n), |n, req| Ok(n + req.m))
    resolver({ n: 40, m: 2 }) == Ok(42.U64)
}

expect {
    resolver = Cofx.and_then(|_req| Ok(1.U64), |_n, _req| Err("second failed"))
    resolver({}) == Err("second failed")
}

expect {
    resolver = Cofx.and_then(|_req| Err("first failed"), |n, _req| Ok(n + 1.U64))
    resolver({}) == Err("first failed")
}

# Arrow-call pipeline: x->f(y) is f(x, y), so combinators chain without
# nesting. Pure all the way through, so it runs right here in an expect.
expect {
    resolver = (|req| Ok(req.n))
        ->Cofx.map(|n| n * 2)
        ->Cofx.resolve(|n| Ok(n + 1))
        ->Cofx.and_then(|n, req| Ok(n + req.m))
    resolver({ n: 20, m: 1 }) == Ok(42.U64)
}

expect {
    combined = Cofx.map2(|req| Ok(req.n), |req| Ok(req.m), |n, m| n + m)
    combined({ n: 40, m: 2 }) == Ok(42.U64)
}

expect {
    combined = Cofx.map2(|_req| Err("a failed"), |req| Ok(req.m), |n, m| n + m)
    combined({ m: 2.U64 }) == Err("a failed")
}

# --- domain-error binders ---

## bind1 wraps the pure handler's answer in Ok
expect {
    bound = Cofx.bind1(|req| Ok(req.n), |n| n + 1)
    bound({ n: 41 }) == Ok(42.U64)
}

## bind1 passes a domain-error rejection through as Err
expect {
    bound = Cofx.bind1(|_req| Err(NotFound("nope")), |n| n + 1.U64)
    bound({}) == Err(NotFound("nope"))
}

## bind2 resolves left to right and combines through the handler
expect {
    bound = Cofx.bind2(|req| Ok(req.n), |req| Ok(req.m), |n, m| n + m)
    bound({ n: 40, m: 2 }) == Ok(42.U64)
}

## bind2's first rejection wins and the handler never runs
expect {
    bound = Cofx.bind2(|_req| Err(Boom), |req| Ok(req.m), |n, m| n + m)
    bound({ m: 2.U64 }) == Err(Boom)
}
