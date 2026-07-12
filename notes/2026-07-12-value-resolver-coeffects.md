# Design log: coeffects as values (resolver functions), replacing type dispatch

**Date:** 2026-07-12
**Status:** implemented (same day); superseded same day by the nominal
`Resolver` pipeline — see the follow-up section at the bottom.
**Supersedes:** the coeffect *binding mechanism* of
[2026-07-11-coeffect-effect-handlers-design-log.md](2026-07-11-coeffect-effect-handlers-design-log.md).
Everything else from that design — pure `->` handlers, reads-before /
writes-after, the host memo registry, the effect executor — stands unchanged.

## Problem

Coeffects will carry most of a real app's request-handling logic, so they need
the reuse story Re-frame gets from composition and higher-order functions:
parameterized middleware-like pieces (`require_role("admin")`,
`paginated(50)`), dependent reads (session → user), shared reads resolved
once.

The shipped mechanism could not express any of that. `Ctx.argsN` recovered
each coeffect's resolver **from the handler's parameter type** (`where
[a.inject! : req => Try(a, resp)]`). Selection-by-type leaves no argument
channel: a type cannot carry `"admin"` (no type-level literals), so a nominal
coeffect has exactly one fixed `inject!`. Worse, the per-type static memo
`key : Str` would *alias* across parameterizations if one ever faked
parameters via closures.

A considered alternative — a second "interceptor" layer beside typed
coeffects — was rejected in discussion: Re-frame's injectors mostly just set
up coeffects, and functions already are the "single mechanism with two very
different shapes". A guard layer wouldn't fix the unparameterizable coeffect
it stands next to.

## Decision

Go all-in on **coeffects as values**. A coeffect is a plain resolver
function:

    resolver : req => Try(a, Response)

- `Ctx.with0..with5` replace `Ctx.args0..args5`: routes name their resolvers
  explicitly, and the pure handler receives the **raw** resolved values — the
  nominal wrapper types (and their destructuring / `.value()`) are deleted.
- **Parameterization** = partial application: `Req.param_named("id")`,
  `Req.query_named(n)`, `Req.header_named(n)` are factories returning
  resolvers; app factories look identical.
- **Composition** = `Ctx.map` / `Ctx.and_then` (dependent resolver gets the
  resolved value *and* the request). `examples/pure`'s `fetched_user!` is the
  showcase: `Ctx.and_then(Req.param_named("id"), load_row)`.
- **Memoization** is opt-in, not ambient: only non-idempotent or shared
  resolvers route through `Ctx.get!`, and the key must include the resolver's
  parameters (fixes the static-key aliasing hazard by construction). Built-in
  pure projections (`body`, `headers`, `model`, …) skip the memo — which also
  makes them **pure and expect-testable**, restoring unit coverage the
  `inject!`-only API had lost. `Req.reserved_id!` keeps the memo (run-once).
- Naming signals purity: pure resolvers unmarked, effectful ones carry `!`
  (compiler-enforced).

Cost accepted: routes spell out their resolvers
(`Ctx.with2(Req.reserved_id!, Req.json_body, create_user)`) instead of
inferring them from the handler signature. This reverses the Phase-4
type-dispatch decision *for binding only*; typed positional handler args
survive intact.

## Non-changes

Pure-Roc-layer only. The route handler crossing the FFI is still
`Box(req => resp)`; no glue regen, no host change, no ABI change. `Ctx.get!`,
the host memo registry, `Res.fx*`, and the effect executor are untouched.

## Compiler findings (new Zig compiler)

1. **Pure→effectful coercion works at argument positions**: a pure lambda
   passes where `req => Try(a, resp)` is expected. This is what lets one
   `withN` signature serve both pure and effectful resolvers.
2. **`=>`-annotated returns poison expects**: annotating `map`/`and_then` as
   returning `req => …` made even pure compositions uncallable in `expect`.
   Leaving them **unannotated** keeps them purity-polymorphic — testable with
   pure resolvers, effectful with effectful ones. Their shapes are documented
   in doc comments instead.
3. **Cross-module expect row-freeze**: an expect in `Req.roc` applying
   `Ctx.map(Req.param_named("id"), …)` to a closed record literal froze
   `param_named`'s request row closed for downstream apps (route lists then
   failed to unify with the full request record). Same-module factory expects
   with closed records are fine. Rule of thumb: don't expect-test a sibling
   module's combinator applied to this module's factory; leave composition
   coverage to the e2e gate.
4. **Effectful values need `!` names** even when defined by application
   (`fetched_user! = Ctx.and_then(…)`), and a top-level resolver defined by
   applying a combinator compiles fine.

## Follow-up (same day): nominal `Resolver` for a pipelined surface

Bare-function resolvers compose only by nesting (`Ctx.and_then(Ctx.map(…)`)
— and the Zig compiler dropped `|>` (`OpPizza` canonicalizes to "unsupported
operator"), while its replacement, method chaining, dispatches only on
NOMINAL receivers. Since real apps will lean hard on multi-step coeffects,
we accepted the wrapper: `platform/Resolver.roc` defines

    Resolver(req, a, resp) := [Resolver(req => Try(a, resp))].{
        from, run!, map, and_then, map2 }

- **Pipelines**: `Req.param_named("id").and_then(load).map(shape)` — chaining
  on call results works (same mechanism as `Res.fx(…).opt(…)`).
- **Record builder**: the new compiler's dormant `{ a: ra, b: rb }.TypeName`
  syntax desugars through `TypeName.map2` — with `Resolver.map2` defined,
  `{ a: Req.reserved_id, b: Req.reserved_id }.Resolver` builds a resolver of
  a named record. Verified at runtime (spike + pure e2e, including memo
  dedup underneath). ≥2 fields required. This is Re-frame's coeffect *map*.
- `Ctx.withN` now take `Resolver` values; `Req` built-ins are pre-wrapped so
  route spellings barely change; handlers still receive raw values.

Costs accepted:
- Resolver values are no longer expect-testable at all (the payload is typed
  `=>`, so even pure ones count as effectful). Pure `Req.*` helpers keep the
  unit coverage; resolver behavior is e2e-covered.
- The `!` purity marker is gone from resolver names (`reserved_id!` →
  `reserved_id`): a wrapped resolver is a pure value carrying a
  possibly-effectful function.

Two more value-restriction findings (top-level VALUES, unlike function
definitions, cannot carry unresolved constraints):
1. A method-constrained type var errors outright ("POLYMORPHIC VALUE"):
   `json_body`'s decode target has `a.parser_for`, so it must stay a FACTORY
   — `Req.json_body()`.
2. An unpinned numeral DEFAULTS silently: `reject`'s `400`/`500` became
   `status: Dec` inside wrapped values, breaking unification with the app's
   `U16` response — fixed by pinning `code : U16` inside `reject`.

## Follow-up 2 (same day): `->` arrow calls make the wrapper unnecessary

The user spotted that the compiler kept a UFCS-style **arrow-call operator**:
`x->f(y)` canonicalizes to `f(x, y)` (receiver becomes the FIRST argument;
`Can.zig`'s `arrow_call`, fully implemented — unlike the removed `|>`). It
works on plain functions, chains, and needs no nominal receiver — which
falsifies the premise of the nominal `Resolver` ("chaining needs a nominal").

Reverted to **bare-function resolvers** with `->` pipelines:

    fetched_user! = Req.param_named("id")
        ->Resolver.and_then(|id, req| load_row!(id, req))

`Resolver` survives as a PHANTOM nominal (`Resolver := [].{ map, and_then,
map2 }`): its members are ordinary combinators over bare functions, and its
name gives the record builder its `map2` lookup — `{ a: ra, b: rb }.Resolver`
combines bare resolvers into a bare resolver of a named record; nothing is
ever wrapped. Runtime-verified (spike + pure e2e with memo dedup).

This undoes every cost of the nominal iteration: resolver expects are back
(56 total), `!` purity markers are back (`reserved_id!`), `json_body` is a
plain value again (bare function definitions generalize; only top-level
non-function VALUES hit the value restriction), and `from`/`run!` are gone.
The `code : U16` pin in `reject` is kept as cheap defense.

Residual quirk: APP-local factories share weak type vars across uses within
the app — an expect applying one to a toy record conflicts with route uses at
the full request type. Platform-module factory expects are unaffected.

## Follow-up 3 (same day): one `Cofx` module; factories get the short names

- `Ctx` + `Resolver` merged into **`Cofx`** (the Re-frame term): memo `get!`,
  binders `with0..with5`, combinators `map` / `resolve` / `and_then` / `map2`.
  The record builder is now `{ a: ra, b: rb }.Cofx`.
- New **`Cofx.resolve`**: `and_then` minus the request — the dependent step
  receives only the resolved value, which is the common case (the pure
  example's `user_from!` pipeline had been ignoring `req`).
- `Req` name swap: the route-facing coeffect factories own the short names —
  `Req.param(n)` / `Req.query(n)` / `Req.header(n)` — and the direct pure
  helpers are `get_param` / `get_query` / `get_header`. The `_named` suffix
  had been collision-avoidance, not communication.
