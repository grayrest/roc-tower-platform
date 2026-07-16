app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Cofx

# SPIKE I1 fixture: per-request coeffect memoization. No Port setting so the
# PORT env var controls binding (e2e harness picks a free port).
Model : {}

Request : {
    ctx : U64,
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    model : Box(Model),
}

Response : Res.Response

main = |{}| {
    settings: [],
    init!: init!,
    routes: [
        # Assert 1+2: resolve the SAME non-idempotent coeffect twice in one
        # request; concurrent requests must each reserve a distinct value.
        { method: "GET", path: "/memo", handler: Box.box(memo!) },
        # Global injector run count, for before/after deltas.
        { method: "GET", path: "/runs", handler: Box.box(runs!) },
        # Assert 3: live request-store count (this request itself counts as 1).
        { method: "GET", path: "/registry", handler: Box.box(registry!) },
        # Assert 4: coeffect A resolves + stores, then coeffect B rejects.
        { method: "GET", path: "/reject", handler: Box.box(reject!) },
        # Assert 5: coeffect A resolves + stores, then the host panics.
        { method: "GET", path: "/panic", handler: Box.box(panic!) },
        # Bisection probes for the /reject SIGABRT.
        { method: "GET", path: "/trystore", handler: Box.box(trystore!) },
        { method: "GET", path: "/strstore", handler: Box.box(strstore!) },
        { method: "GET", path: "/twostores", handler: Box.box(twostores!) },
    ],
}

init! : () => Try(Model, Str)
init! = || Ok({})

# The non-idempotent injector: reserving an id must happen exactly once per request.
reserve! : {} => U64
reserve! = |{}| Host.reserve_id!()

memo! : Request => Response
memo! = |req| {
    first = Cofx.get!(req.ctx, "reserved_id", reserve!)
    second = Cofx.get!(req.ctx, "reserved_id", reserve!)
    Res.text(200, "ctx=${req.ctx.to_str()};first=${first.to_str()};second=${second.to_str()}")
}

runs! : Request => Response
runs! = |_req| Res.text(200, "runs=${Host.reserve_runs!().to_str()}")

registry! : Request => Response
registry! = |_req| Res.text(200, "registry=${Host.ctx_registry_size!().to_str()};live_boxes=${Host.memo_live_boxes!().to_str()}")

# Coeffect B: resolution itself fails, so the value stored is a Try Err and
# the handler rejects with a 403 AFTER coeffect A already stored a box.
failing! : {} => Try(U64, Str)
failing! = |{}| Err("b is unavailable")

reject! : Request => Response
reject! = |req| {
    a = Cofx.get!(req.ctx, "reject_a", reserve!)
    match Cofx.get!(req.ctx, "reject_b", failing!) {
        Ok(_) => Res.text(200, "unexpected: b resolved")
        Err(_) => Res.text(403, "rejected after a=${a.to_str()}")
    }
}

# Bisection: store ONLY the Try-valued coeffect.
trystore! : Request => Response
trystore! = |req| {
    match Cofx.get!(req.ctx, "try_b", failing!) {
        Ok(_) => Res.text(200, "b resolved")
        Err(_) => Res.text(403, "b failed as expected")
    }
}

# Bisection: store a Str-valued coeffect.
greeting! : {} => Str
greeting! = |{}| "hello-${Host.reserve_id!().to_str()}"

strstore! : Request => Response
strstore! = |req| {
    g = Cofx.get!(req.ctx, "greeting", greeting!)
    Res.text(200, "g=${g}")
}

# Bisection: store TWO U64 coeffects in one request.
twostores! : Request => Response
twostores! = |req| {
    a = Cofx.get!(req.ctx, "two_a", reserve!)
    b = Cofx.get!(req.ctx, "two_b", reserve!)
    Res.text(200, "a=${a.to_str()};b=${b.to_str()}")
}

panic! : Request => Response
panic! = |req| {
    a = Cofx.get!(req.ctx, "panic_a", reserve!)
    Host.panic_test!()
    Res.text(200, "unreachable a=${a.to_str()}")
}
