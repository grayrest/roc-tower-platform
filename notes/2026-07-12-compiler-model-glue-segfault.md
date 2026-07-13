# Compiler crash: uppercase `Model` in a requires signature segfaults `roc glue`

**Found**: during platform bring-up (originally surfaced as a
"missing lowering visibility artifact" crash); minimal repro extracted
2026-07-12 against `release-fast-252a59ae`, where it is a SIGSEGV
(fault address 0x1238; build a Debug/ReleaseSafe compiler for a stack
trace — ReleaseFast disables tracing).
**Status**: worked around everywhere (tower-platform uses lowercase
`model` in all requires type positions); queue for a compiler session.

## Symptom

Referencing the requires binder by its UPPERCASE name (`Model`) in a type
position *inside the requires signature* passes `roc check` cleanly but
segfaults `roc glue`. The lowercase rigid (`model`) works. Notably, using
`Box(Model)` in an ordinary top-level annotation OUTSIDE the requires
block (e.g. `init_for_host!`) is fine — only the in-requires reference
crashes.

## Reproducer

`main.roc` (complete platform file; the only load-bearing character is the
`M` in `Try(Model, Str)` — lowercase it and glue succeeds):

```roc
platform ""
    requires {
        [Model : model] for main : {} -> {
            init! : () => Try(Model, Str),
        }
    }
    exposes []
    packages {}
    provides {
        "roc_init": init_for_host!,
    }
    targets: {
        inputs_dir: "targets/",
        arm64mac: { inputs: ["libhost.a", app] },
    }

init_for_host! : () => Try(Box(Model), Str)
init_for_host! = || {
    program = main({})
    match (program.init!)() {
        Ok(model) => Ok(Box.box(model))
        Err(message) => Err(message)
    }
}
```

Run (no app, no targets/ contents needed):

```sh
roc check main.roc                              # No errors found
roc glue src/glue/src/RustGlue.roc out/ main.roc # SIGSEGV, fault addr 0x1238
```

Both `Try(Model, Str)` and `Box(Model)` inside the requires signature
trigger it; `roc check` never notices, so the crash only appears at glue
time.

## Expected

Either uppercase `Model` in requires signatures is legal (it names the same
rigid as `model`) and glue should handle it, or it's illegal and `roc check`
should report it — anything but a type-checked platform that segfaults the
glue pass.
