## Per-request coeffect resolution with host-backed memoization.
##
## `get!` resolves a coeffect at most once per REQUEST, keyed by the host-minted
## ctx id (`req.ctx`) plus an author-supplied `Str`. On a miss it runs the
## injector and stores a boxed THUNK capturing the result (the host can release
## an erased callable soundly at teardown; it cannot release an opaque value
## box). On a hit it retrieves the stored thunk and invokes it, which is a
## static-dispatch-known downcast back to `a` — memory-safe only because the
## key uniquely identifies the type within one request.
Ctx := [].{
    ## Resolve `key`'s coeffect for request `ctx`, running `inject!` only on
    ## the first call within that request.
    get! : U64, Str, ({} => a) => a
    get! = |ctx, key, inject!| {
        match Host.memo_get!(ctx, key) {
            Ok(boxed) => {
                thunk = Box.unbox(boxed)
                thunk({})
            }
            Err(Miss) => {
                value = inject!({})
                Host.memo_put!(ctx, key, Box.box(|{}| value))
                value
            }
        }
    }
}

import Host
