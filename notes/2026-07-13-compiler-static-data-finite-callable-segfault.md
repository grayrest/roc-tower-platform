# Compiler crash: `roc build` of realworld segfaults in static-data export of aggregated boxed handlers

**Compiler:** `release-fast-8a2a90a5` (roc-lang/roc `8a2a90a532`), also reproduced on `20fb2436`.

## ✅ RESOLVED (2026-07-13)

Fixed in roc: branch `fix-erased-callable-static-data-ambiguous-template`, commit `b697ad5001` — `erasedFnEntry` (src/compile/static_data_exports.zig) now disambiguates same-template erased-callable entries by which entry's finite-callable capture slots admit the value's captured functions, instead of taking the first template match. realworld now builds and passes **154/154** conformance; hello/e2e/ctxmemo unaffected. The `zig-out/bin/roc` used by this repo's justfile is built from that branch. (`examples/repro/` remains a SEPARATE, still-open check-time bug — `erroneous checked type reached Monotype instantiation` — unrelated to this fix.)

The original investigation follows.

**Original status:** blocked `just realworld-conformance` (the whole examples/realworld end-to-end path). `roc check` passes; only `roc build` crashed.

## Symptom

```
$ roc build examples/realworld/main.roc
Segmentation fault (SIGSEGV) in the Roc compiler.   # ReleaseFast
# ReleaseSafe: thread panic: reached unreachable code
```

`roc check examples/realworld/main.roc` → `No errors found`. The crash is release-only and does **not** reproduce in a Debug build (it only reaches the static-data-export phase after full monomorphization+codegen; Debug is too slow to hit it in a reasonable time, and the bug lives in the `noreturn` invariant path).

## Exact invariant

Instrumenting `staticDataInvariant` (src/compile/static_data_exports.zig:1199) to print its message before the `unreachable`:

```
SDINV: finite callable ConstStore function was absent from LIR callable variants
```

Call site: **`fnVariantForConstFn`, src/compile/static_data_exports.zig:790** —

```zig
fn fnVariantForConstFn(set: lir.Program.FnSet, fn_value: ConstFn) lir.Program.FnVariant {
    for (set.variants) |variant| {
        if (sameFnTemplate(fn_value, variant.template)) return variant;
    }
    staticDataInvariant("finite callable ConstStore function was absent from LIR callable variants");
}
```

A `ConstFn` present in the checked `ConstStore` static data has **no matching variant** in the LIR's `FnSet.variants`, so the finite-callable materializer can't resolve it.

> Note: in ReleaseFast/ReleaseSafe the crash *symbolicates* to `Check.zig:5833 hoistSelectionInvariant` — that is an **ICF artifact** (identical `noreturn`→`unreachable` functions get merged). The real function is `staticDataInvariant` reached via `fnVariantForConstFn`. Ignore the `hoistSelectionInvariant` symbol.

## Backtrace (ReleaseSafe)

```
rocBuildLlvm (cli/main.zig:8191)
 → static_data_exports.buildProvidedDataExports
 → materializeValue → materializeRawValue → writeBox → writeValue
 → materializeErasedFn → writeCaptures (struct branch)
 → writeValue  → fnVariantForConstFn → staticDataInvariant   # crash
```

So: materializing the static `main` value → its `routes` list → a boxed erased-fn handler → **writing that handler's captures** → one captured value is a **finite callable** (a defunctionalized function value) whose variant is missing from the LIR.

## Root cause (mechanism)

realworld's route handlers are boxed closures that capture function values:

```roc
tags_h! = Box.box(Cofx.with1(tags!, tags))   # Cofx.with1 = |resolve!, handler| |req| ...
```

`Box.box(...)` erases the outer closure, but the closure **captures `tags!` and `handler` as function values**. Those captured functions are *finite callables*. When the whole `main` record is materialized as compile-time static data, `writeCaptures` materializes each captured function via `writeFnValue` → `fnVariantForConstFn`, which looks the function up in the LIR's `FnSet.variants` by `sameFnTemplate`.

**The lookup misses, and the miss is total.** Instrumenting `fnVariantForConstFn` on the realworld build (self-aborting after the first miss to avoid ReleaseFast UB-after-`unreachable`):

```
FNVAR-MISS: variants=2
  variant[0]: artifact=false proc_base=false template=false
  variant[1]: artifact=false proc_base=false template=false
```

The set has only **2 variants**, and the stored `ConstFn` (a `local_template`) differs from both in **every** ProcTemplate field — including `artifact` (the owning **module** digest). So the captured function is from a *different module* than either variant: the finite-callable set simply **does not contain it**.

**Where the set comes from:** `solved_lir_lower.fnSetForType` (src/postcheck/solved_lir_lower.zig:1655) builds `FnSet.variants` verbatim from the type's finite-callable variant span (`self.types.fnVariantSpan(variants_span)`). It faithfully lowers whatever the type declares. So the incompleteness is **upstream in the type system**: the finite-callable union type for the aggregated (cross-module `.concat`ed) route list was formed with only 2 members, omitting handler functions from other modules — yet const-eval placed one of those omitted functions into the materialized data.

This is a bug in the (new) "Materialize compile-time constants as static data" feature — "Materialize finite callable static data" (`42ec5673a6`) / "Cover erased callable static data" (`7f07763e9f`) / "Cover aggregate static data exports" (`4348e87a`). The feature did not exist at `252a59ae`, where realworld builds cleanly via runtime construction (bisect-verified good).

## Reduction

- `roc build` of realworld: **crashes**.
- Each route group in isolation (`Users.routes`, `Profiles.routes`, `Articles.routes`, `Comments.routes`, inline `tags`): **builds cleanly**.
- Only the **concatenated aggregate** crashes → matches "Cover *aggregate* static data exports".
- `examples/hello`, `examples/e2e`, `examples/ctxmemo` build fine. ctxmemo boxes **bare top-level fns** (`Box.box(memo!)`, no captures) — no `writeCaptures` path.
- A single-file two-handler app (`with1` + `with2`) and a cross-module route-concat app both **build cleanly** — so the trigger needs realworld's specific handler variety/variant set, not merely "two capture shapes concatenated." A minimal <30-line repro was not isolated; `examples/realworld/main.roc` is the reliable repro.

## Type-checker investigation (lambda-set solving)

Traced upstream through the lowering:
- `fnSetForType` → `lowerFnMembers` (solved_lir_lower.zig:1132) produce exactly one variant per **solved lambda-set member** (`self.solved.types.memberSpan`). So the 2-variant set faithfully reflects a **2-member solved lambda set** — the incompleteness is in the *solved* set, upstream.
- Lambda sets are solved in `src/postcheck/lambda_solved/solve.zig`. The set-merge on unification, `mergeLambdaSets` (solve.zig:1721), is **correct** — it unions lhs+rhs, dedups by `.lambda` symbol, and unifies captures of shared members. So members are **not** dropped at merge.
- Searching **all** `FnSet`s at the crash (diagnostic in `writeFnValue`) pinpointed it exactly:

  ```
  SETMISS: plan_set=0 plan_variants=2 found_in_set=49 sets_containing_fn=1 total_sets=51
  ```

  The stored function **is** present — correctly — in `FnSet #49`. The const node for this capture position, however, is typed to `FnSet #0` (2 members), which does not contain it. So the function is **not** missing from the program (lambda-set solving produced a complete set for it, #49); the bug is that **this capture position was assigned the incomplete lambda-set type (#0) and never unified with the complete one (#49)** that legitimately holds the function.

## Suggested fix direction

A deeper diagnostic (`plan_set` vs the set that actually holds the fn) refined this further:

```
SETMISS plan_set=0(2 variants) host_set=49
  host_set has 1 variants; same layout as plan? false
  plan_set ⊆ host_set? false
```

`host_set #49` has exactly **1** member: the function itself (its own singleton lambda set, as minted by `functionType` in solve.zig:229). The capture's plan points to `#0 = {two OTHER functions}`. And (earlier diagnostic) the stored fn's `artifact` **module** digest differs from both #0 members. So:

**`Cofx.withN`'s inner closure is shared/monomorphized; its `handler`-capture lambda set unified the handlers from some modules into `#0 = {A, B}`, but THIS route's handler `C` (from a different module) never unified into that shared set — it stayed a singleton (`#49`). The route's capture const-node nonetheless references `#0`, which does not contain `C`.** It's a **cross-module lambda-set unification gap**: cross-module route handlers passed to the shared `Cofx.withN` combinator don't merge into its capture set (or the const-node/monomorphized-instance selection for a cross-module route points at the wrong `Cofx.withN` capture set).

Fix is in cross-module lambda-set unification / monomorphized-instance selection (`lambda_solved/solve.zig` constraint generation, or the lambda-mono specialization that picks a shared closure instance). A defensive exporter swap is unsafe (#0 and #49 differ in member count → different tag-union layouts). The slot's type/layout must be the correct complete one.

This is genuinely deep cross-module solver/monomorphization work; the diagnosis above is maximally actionable for a roc maintainer.

A purely defensive exporter fix is **not** clean: finite callables are tag-union-encoded (discriminant + per-variant capture layout), so `fnVariantForConstFn` cannot synthesize a materialization for a function whose variant (discriminant, capture layout) is unknown.

Attempted (in this investigation, reverted): giving `fnVariantForConstFn` the same `if (set.variants.len == 1) return set.variants[0]` shortcut that `const_store_writer.selectFnVariant` already uses. That is a legitimate consistency hardening (the writer never validates the template for single-variant sets, so the exporter shouldn't reject one), but it does **not** fix realworld — the crashing set has 2 variants, not 1.

## Investigation notes (build/repro)

- Repro: `roc build examples/realworld/main.roc` (release). `roc check` passes.
- Each route group builds in isolation; only the concatenated aggregate crashes.
- No <30-line minimal repro isolated: same-module 2-handler and cross-module route-concat toy apps build fine; the trigger needs realworld's cross-module handler variety (a captured function whose union type omits it).
- The release backtrace's `hoistSelectionInvariant` symbol is an ICF artifact — the real site is `staticDataInvariant` via `fnVariantForConstFn`.
- Debugging tip: the invariant only prints its message in Debug builds; in ReleaseFast the `unreachable` is UB and execution continues (produced 41MB of garbage). Instrument with an explicit `std.process.exit` to get clean output from a fast ReleaseFast build.
