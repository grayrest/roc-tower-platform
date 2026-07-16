# Compiler crash: a generic nominal-module method forwarding a value to a platform generic (`Res.json`)

**Date:** 2026-07-16 · **roc:** local dev build at `~/Repositories/roc` · macOS aarch64

> ## Re-check 2026-07-16 (after upstream `main` update) — STILL BROKEN, now a clean panic
>
> On the updated compiler the crash is no longer a raw SIGSEGV; it's a Zig
> panic with a message:
>
> ```
> thread panic: postcheck invariant violated: erroneous checked type reached
> Monotype instantiation
>   src/postcheck/monotype/lower.zig:8611  instNodeContent  (.err => Common.invariant(...))
> ```
>
> An **erroneous checked type** (a type-error node) survives `roc check`
> silently and then trips `constrainKnownType` → `constrainTypeToMono` →
> `instNode` in monotype lowering. Two constructs hit the SAME panic, so they're
> one bug:
> 1. `Connect.ok = |value| Res.json(200, value)` called with a record (the
>    generic-forward case below).
> 2. **Annotating** the error helper `error : Connect.Code, Str -> Res.Response`
>    (naming the platform `Res.Response`, whose `effects` is a cross-module
>    inline union, in an example-module signature). Unannotated + a local
>    `text : Str` pin builds.
>
> Both workarounds from the original writeup still apply (inline `Res.json`;
> leave the helper unannotated). **The fold-over-`Try` crash — a separate note,
> `2026-07-15-compiler-fold-nested-try-segfault.md` — is FIXED**: `decode_books`
> now folds a `Try` accumulator and both builds and runs correctly.
>
> Unrelated regression noticed in the same update: **`roc test` crashes on every
> file** (hello / realworld / platform / connect all abort in
> `cli/main.zig:lowerPlannedTestModule`), so `just test-roc` and any `expect`
> evaluation are currently broken independent of app code. `roc check` / `roc
> build` are unaffected.

## Symptom

`roc build examples/connect/main.roc` crashed with exit 139:

```
Segmentation fault (SIGSEGV) in the Roc compiler.
```

`roc check` on the same file **passed** — the crash is in codegen, not
type-checking.

## Trigger (minimal, verified by bisection)

An example-local nominal module (`examples/connect/Connect.roc`) defined a
generic, unannotated pass-through method that forwards its argument to the
platform's generic `Res.json`:

```roc
Connect := [].{
    ok = |value| Res.json(200, value)   # value : any encodable type
}
```

Calling it from a handler with a record argument crashed codegen:

```roc
get_book! = |_req| Connect.ok({ book: "stub" })   # SIGSEGV at build
```

Calling `Res.json` **directly** with the same record does not:

```roc
get_book! = |_req| Res.json(200, { book: "stub" })   # builds fine
```

Bisection facts (each a separate `just build`):

- Skeleton app, handler `Res.text(200, "stub")` → **builds**.
- `import Connect` but don't call it → **builds** (compiling the module is fine).
- Handler `Connect.error(NotFound, "stub")` (a NON-generic method — concrete
  arg types, concrete-shaped result) → **builds**.
- Handler `Connect.ok({ book: "stub" })` (the GENERIC method) → **SIGSEGV**.
- Handler `Res.json(200, { book: "stub" })` (the same platform generic, called
  directly) → **builds**.

So the crash is specific to **specializing a generic method defined on an
example-local nominal type when its body forwards the type variable into
another module's generic function**. A monomorphic method on the same nominal
type is fine; the platform generic called directly is fine.

## Related symptom (expect evaluation)

Before the module boundary was in play, evaluating a call to `Connect.error`
(which returns the platform's `Res.Response`) inside an `expect` also
segfaulted — `roc test` crashed on:

```roc
expect Connect.error(NotFound, "nope").status == 404
```

while an inline `Json.to_utf8({ code: "x", message: "y" })` compared in an
`expect` was fine. This looks like the same family (evaluating/specializing a
call to an example-module method that returns a platform record), surfacing in
the interpreter rather than codegen. Not isolated as tightly as the build repro.

## Workaround (in the demo)

There is no `Connect.ok` wrapper: a Connect unary success is exactly
`Res.json(200, msg)`, so handlers call the platform generic **directly**.
`Connect.error` (non-generic) stays as a module method. The two mapping helpers
(`status_of`/`string_of`) are pure and expect-testable; the `error` wrapper is
covered end-to-end by `bench/connect-check.sh` instead of an `expect`.

## Impact

Cosmetic-to-annoying, not blocking: the idiomatic "thin wrapper that forwards a
polymorphic value to a platform builder" is unavailable across a nominal-module
boundary, but inlining the platform call is a clean substitute. Worth fixing —
this is a natural pattern (an app defining its own `ok`/`json`/`respond` helper
that delegates to `Res.*`).
