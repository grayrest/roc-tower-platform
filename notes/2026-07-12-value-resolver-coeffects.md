# Design log: coeffects as values (resolver functions), replacing type dispatch

**Date:** 2026-07-12
**Status:** implemented (same day)
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
