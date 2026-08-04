# Toolchain catch-up — repairing `main` against roc `release-fast-917abf7f`

**Date:** 2026-08-04 · `just check` exits 0, realworld conformance 154/154.

`main` was red against the stable toolchain (`~/.bin/roc`, built 2026-08-01)
and had been since before the encoder/decoder branch was opened: 18 `roc check`
errors, 3 failing platform expects, 1 failing sqlgen test, 2 clippy errors.
Nothing here is a design change; it is catch-up with compiler and Rust
toolchain movement after the last commit (`8cbb8d2`, 2026-07-18). Found while
gating phase 0 of `plans/2026-08-03-encoders-decoders.md`.

## 1. Effect-carrying `Res.Fx` terminals silently DROPPED their effects

The one that mattered. `Res.Fx.text` / `.html` / `.bytes` / `.no_content` /
`.redirect` each carried an explicit `-> Res.Response` return annotation, and
with that annotation the returned record's `effects` field came back **empty**.
`Res.fx(DbWrite(…)).text(…)` produced a response that ran no write at all.

Isolated by splitting the failing expect: `builder.effects.len() == 1` passes,
`Res.fx(Log(…)).text(201, "ok").effects.len() == 0` also passes — the builder
accumulates correctly and the terminal discards. `status` and `body` survive;
only `effects` is lost. `json` was the sole terminal that worked, and it was
the sole terminal WITHOUT the annotation.

Live blast radius, had it shipped: `Articles.roc:254` and `Comments.roc:104`
both finish a DELETE chain with `.no_content()`, so article and comment
deletion would have been no-ops; `hello/main.roc:103` records no visit;
`e2e/main.roc:205/232/255` are the effect tests. It was NOT shipping broken —
the platform did not compile against this toolchain at all, so the regression
was latent rather than live.

**Fix:** drop the five return annotations. Same family as the existing rule
that app-side `Res.Response` helpers must be unannotated (`Api.field_error`,
`Req.reject`), and as the `Connect.roc:25-27` lowering crash. Comment left at
the top of the terminal block so nobody "tidies" the annotations back in.

## 2. `Json.ParseErr` no longer exists

`MISSING NESTED TYPE — 'Json' is in scope, but it doesn't have a nested type
named 'ParseErr'`. The alias is gone from `Builtin.roc` on roc `origin/main`;
`Json.parse` now fails with the bare open union `[InvalidJson(Str), ..errs]`,
with `MissingRequiredField(Str)` coming from the generated record driver.

**Fix:** six expect annotations in `NullableStr`/`NullableI64` take `_` and let
inference do it; `Req.explain` names the union inline and OPEN —
`List([MissingRequiredField(Str), InvalidJson(Str), ..])` — which also matches
`Sql`'s decoder contract (open so a caller's tags merge). Phase 2 of the
encoder/decoder plan removes this dependency entirely: once the platform
vendors its own JSON format it owns the error union, so there is nothing
upstream left to track.

## 3. Nominals declaring associated items no longer derive `is_eq`

`MISSING METHOD` / `TYPE DOES NOT SUPPORT EQUALITY` wherever a nominal with a
method block was compared with `==`. Confirmed by adding an explicit `is_eq` to
`NullableStr`: 18 errors → 12, its own 10 → 4.

**Fix:** explicit `is_eq` on `NullableStr` and `NullableI64` — both are
compared in expects and by apps reading nullable columns, so the method earns
its place. NOT added to `Sql.Row`: equality there would cascade into
`Host.SqlValue`, and the single expect that needed it (`Sql.first([]) ==
Err(NoRows)`) only tests the `Err` side, so it now `match`es instead.

Worth watching: phases 1-3 of the encoder/decoder plan define more nominals
with method blocks (formats and their states). Any that get compared need
`is_eq` spelled out.

## 4. Stale sqlgen assertion

`emits_one_reader_with_nullable_and_bool` asserted the CLOSED spelling
`[NoRows, DbErr(Str)]`; sqlgen has emitted the OPEN `[NoRows, DbErr(Str), ..]`
since `06f3e35`. A substring assertion, so the trailing `, ..]` made it fail.
The emitted code was right and the test was stale.

## 5. Two new clippy lints on unchanged code

- `doc_lazy_continuation` (effects.rs:415): a doc line beginning `/// > 0)`
  reads as a blockquote marker, making the next line a lazy continuation.
  Reworded to "when nonzero".
- `collapsible_if` (middleware.rs:265): now fires on nested `if let`. Collapsed
  into a let-chain.

## Standing advice

`ROC` defaults to `~/Repositories/roc/zig-out/bin/roc` in the Justfile, which
is a working tree on whatever branch that repo happens to be on — it was on
`write-mask-transform` when this was found, and produced the same 18 errors, so
the two builds happened to agree. They will not always. **Gate runs should pin
`ROC=~/.bin/roc`** (or the Justfile default should point there).
