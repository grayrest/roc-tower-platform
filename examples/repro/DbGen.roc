# Minimal repro for a roc compiler crash (reproduces on release-fast-8a2a90a5,
# 2026-07-14). The REAL trigger is IMPORT PLACEMENT: when an `import` appears
# AFTER a nominal `X := [].{}` block, a qualified reference (`Host.now_ms!`)
# inside one of the block's associated methods does not bind to that import.
# The checker records a phantom, unresolved `Host` import; the external lookup
# resolves to `Content.err` with NO diagnostic, and monomorphization then hits
# `Common.invariant("erroneous checked type reached Monotype instantiation")`
# (src/postcheck/monotype/lower.zig). The crash only surfaces once the module
# is USED from another module (as here, main.roc calls `DbGen.f!`).
#
#   roc check --jobs=1 examples/repro/main.roc   # deterministic SIGSEGV/SIGABRT
#
# THE FIX (app-side): move `import pf.Host` ABOVE `DbGen := [].{` and it checks
# cleanly. (sqlgen was updated to emit imports before the block for exactly
# this reason.) The compiler bug remains: an import after a nominal block
# should still resolve — or error clearly — not silently produce `.err`.
DbGen := [].{
    f! : U64 => Str
    f! = |ctx| {
        _ = Host.now_ms!()
        "x"
    }
}
import pf.Host
