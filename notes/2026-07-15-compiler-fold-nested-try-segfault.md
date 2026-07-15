# Compiler SIGSEGV: `fold` over a `Try` accumulator with a nested `?` closure

**Date:** 2026-07-15 · **roc:** local dev build at `~/Repositories/roc` · macOS aarch64

## Symptom

`roc check` / `roc build` on `examples/realworld/main.roc` crashed:

```
Segmentation fault (SIGSEGV) in the Roc compiler.
Fault address: 0x879
```

during type-checking (not codegen — `roc check` alone crashes).

## Trigger

Introduced in the Phase 3 typed-list work (`Db.run_article_page_typed!`). The
crashing form folded typed rows into a `Try` accumulator, using `?` **inside a
nested `and_then` closure**:

```roc
articles = rows.fold(Ok([]), |acc, row|
    acc.and_then(|list| {
        summary = article_summary_from_row(row)?   # <-- ? inside nested closure
        Ok(list.append(summary))
    }))?
```

Here `article_summary_from_row : Sql.Row -> Try(Db.ArticleSummary, Res.Response)`
— the error arm is a **record type** (`Api.error` → a Response), the ok arm a
nested record (`ArticleSummary` with a `Db.Profile` and `List(Str)`).

## Workaround (shipped)

Rewriting the same logic with explicit `match` (no nested-closure `?`) compiles
and runs cleanly — 154/154 conformance:

```roc
articles = rows.fold(Ok([]), |acc, row| match acc {
    Err(e) => Err(e)
    Ok(list) => match article_summary_from_row(row) {
        Err(e) => Err(e)
        Ok(summary) => Ok(list.append(summary))
    }
})?
```

## Minimal repro — NOT yet isolated

A stripped `fold(Ok([]), |acc, n| acc.and_then(|list| { v = read_cell(n)?; ... }))`
over `Try(_, [Bad])` does **not** crash on its own (checked in `module`,
type-module, and `:= [].{}` forms). So the trigger is more specific than the
syntactic shape — likely the **record-typed error arm** (`Res.Response`) and/or
the nested ok record interacting with `?`-desugaring inside a nested closure.
Reducing it further is open work for whoever fixes the compiler.
