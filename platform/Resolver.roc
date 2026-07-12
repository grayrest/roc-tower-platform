## Coeffect resolver combinators.
##
## A coeffect is a PLAIN FUNCTION — `resolver : req => Try(a, resp)` — that
## either produces the value a pure handler wants or rejects with the response
## to send instead. There is no wrapper type: parameterizing a resolver is
## partial application (`Req.param_named("id")`), reusing one is naming it,
## and PIPELINES use the arrow-call operator, which feeds the left value in
## as the first argument (`x->f(y)` is `f(x, y)`):
##
##     fetched_user! = Req.param_named("id")
##         ->Resolver.and_then(|id, req| load_user!(id, req))
##         ->Resolver.map(|user| user.name)
##
## Independent resolvers combine into a NAMED block with the record-builder
## syntax, which looks up `map2` on this type (that lookup is the only reason
## `Resolver` is a nominal — the fields hold bare resolver functions and the
## result is a bare resolver function):
##
##     inputs = {
##         id: Req.reserved_id!,
##         body: Req.json_body,
##     }.Resolver
##     # : req => Try({ id : I64, body : ... }, resp) — fields resolve in
##     # order, first rejection wins. Requires at least two fields.
##
## The combinators are left unannotated so purity stays inferred: composing
## pure resolvers yields a pure (expect-testable) resolver; effectful
## resolvers (named with `!`) yield effectful ones. Shapes:
##
##     map      : (req => Try(a, resp)), (a -> b) -> (req => Try(b, resp))
##     and_then : (req => Try(a, resp)), (a, req => Try(b, resp)) -> (req => Try(b, resp))
##     map2     : (req => Try(a, resp)), (req => Try(b, resp)), (a, b -> c) -> (req => Try(c, resp))
Resolver := [].{
    ## Transform the resolved value with a pure function; rejections pass
    ## through untouched.
    map = |resolver!, transform| |req| match resolver!(req) {
        Ok(value) => Ok(transform(value))
        Err(rejection) => Err(rejection)
    }

    ## Chain a dependent step: `next!` receives the resolved value AND the
    ## request, so a dependent read can reach the model, other params, or
    ## `Ctx.get!` for memoization.
    and_then = |resolver!, next!| |req| match resolver!(req) {
        Ok(value) => next!(value, req)
        Err(rejection) => Err(rejection)
    }

    ## Combine two independent resolvers (in order, first rejection wins).
    ## This is the record-builder hook: `{ a: ra, b: rb }.Resolver` desugars
    ## into nested `map2` calls.
    map2 = |resolver_a!, resolver_b!, combine| |req| match resolver_a!(req) {
        Err(rejection) => Err(rejection)
        Ok(a_value) => match resolver_b!(req) {
            Err(rejection) => Err(rejection)
            Ok(b_value) => Ok(combine(a_value, b_value))
        }
    }
}

# --- combinators (pure resolvers are callable in expects) ---

expect {
    resolver = Resolver.map(|req| Ok(req.n), |n| n + 1)
    resolver({ n: 41 }) == Ok(42.U64)
}

expect {
    resolver = Resolver.map(|_req| Err("nope"), |n| n + 1.U64)
    resolver({}) == Err("nope")
}

expect {
    resolver = Resolver.and_then(|req| Ok(req.n), |n, req| Ok(n + req.m))
    resolver({ n: 40, m: 2 }) == Ok(42.U64)
}

expect {
    resolver = Resolver.and_then(|_req| Ok(1.U64), |_n, _req| Err("second failed"))
    resolver({}) == Err("second failed")
}

expect {
    resolver = Resolver.and_then(|_req| Err("first failed"), |n, _req| Ok(n + 1.U64))
    resolver({}) == Err("first failed")
}

# Arrow-call pipeline: x->f(y) is f(x, y), so combinators chain without
# nesting. Pure all the way through, so it runs right here in an expect.
expect {
    resolver = (|req| Ok(req.n))
        ->Resolver.map(|n| n * 2)
        ->Resolver.and_then(|n, req| Ok(n + req.m))
    resolver({ n: 20, m: 2 }) == Ok(42.U64)
}

expect {
    combined = Resolver.map2(|req| Ok(req.n), |req| Ok(req.m), |n, m| n + m)
    combined({ n: 40, m: 2 }) == Ok(42.U64)
}

expect {
    combined = Resolver.map2(|_req| Err("a failed"), |req| Ok(req.m), |n, m| n + m)
    combined({ m: 2.U64 }) == Err("a failed")
}
