## A coeffect as a chainable value.
##
## `Resolver(req, a, resp)` wraps a resolver function `req => Try(a, resp)`:
## it either produces the value a pure handler wants, or rejects with the
## response to send instead. The nominal wrapper exists to give resolvers a
## PIPELINED composition surface (Roc dropped `|>`; method chaining needs a
## nominal receiver):
##
##     fetched_user = Req.param_named("id")
##         .and_then(|id, req| load_user!(id, req))
##         .map(|user| user.name)
##
## Independent resolvers combine into a NAMED block with the record-builder
## syntax, which desugars through `Resolver.map2`:
##
##     inputs = {
##         id: Req.reserved_id,
##         body: Req.json_body,
##     }.Resolver
##     # : Resolver(req, { id : I64, body : ... }, resp) — resolves left to
##     # right, first rejection wins. Requires at least two fields.
##
## Handlers receive RAW resolved values (`Ctx.withN` unwraps); parameterizing
## a resolver is partial application (`Req.param_named("id")`); reuse is
## naming the value. NOTE: a wrapped resolver is a pure VALUE carrying a
## possibly-effectful function, so names take no `!` and `expect` cannot run
## one — resolver behavior is covered by the e2e suite.
Resolver(req, a, resp) := [Resolver(req => Try(a, resp))].{
    ## Wrap a resolver function. Pure functions coerce, so one constructor
    ## serves both pure projections and effectful reads:
    ##
    ##     session = Resolver.from(|req| lookup_session!(req))
    from : (req => Try(a, resp)) -> Resolver(req, a, resp)
    from = |f!| Resolver(f!)

    ## Run the resolver against a request. This is the ONLY effectful door;
    ## `Ctx.withN` calls it once per handler argument.
    run! : Resolver(req, a, resp), req => Try(a, resp)
    run! = |resolver, request| match resolver { Resolver(f!) => f!(request) }

    ## Transform the resolved value with a pure function; rejections pass
    ## through untouched.
    map : Resolver(req, a, resp), (a -> b) -> Resolver(req, b, resp)
    map = |resolver, transform| Resolver(|request| match resolver.run!(request) {
        Ok(value) => Ok(transform(value))
        Err(rejection) => Err(rejection)
    })

    ## Chain a dependent step: `next!` receives the resolved value AND the
    ## request, so a dependent read can reach the model, other params, or
    ## `Ctx.get!` for memoization.
    and_then : Resolver(req, a, resp), (a, req => Try(b, resp)) -> Resolver(req, b, resp)
    and_then = |resolver, next!| Resolver(|request| match resolver.run!(request) {
        Ok(value) => next!(value, request)
        Err(rejection) => Err(rejection)
    })

    ## Combine two independent resolvers (left to right, first rejection
    ## wins). This is the record-builder hook: `{ a: ra, b: rb }.Resolver`
    ## desugars into nested `map2` calls.
    map2 : Resolver(req, a, resp), Resolver(req, b, resp), (a, b -> c) -> Resolver(req, c, resp)
    map2 = |resolver_a, resolver_b, combine| Resolver(|request| match resolver_a.run!(request) {
        Err(rejection) => Err(rejection)
        Ok(a_value) => match resolver_b.run!(request) {
            Err(rejection) => Err(rejection)
            Ok(b_value) => Ok(combine(a_value, b_value))
        }
    })
}
