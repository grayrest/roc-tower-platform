# Resolved: a stale vendored glue spec calls erased callables with too few arguments

A single-route app whose handler returns a `Res.Response` crashed the host with
`EXC_BAD_ACCESS` inside `roc_llvm_rc_decref`, on the way out of the handler.
The value being decref'd was consistently `0x88` = 136 = `sizeof(RocRequest)`.
Every endpoint of `examples/realworld` was affected too.

**This is not a compiler bug.** `vendor/RustGlue.roc` is older than the compiler
that builds the apps, and the erased-callable ABI gained parameters in between.
The host calls every `Box.box`'d handler with four registers; the compiler emits
procs that read five. The struct size was a red herring: 136 is just what the
host happened to leave in `x4` from its own record copy.

Root cause found and verified 2026-08-31. Keep this app around as a regression
check for the boundary call.

## The mismatch

`vendor/RustGlue.roc` declares:

```rust
pub type RocErasedCallableFn = extern "C" fn(*mut RocHost, *mut u8, *const u8, *mut u8);
```

The compiler's own spec at `src/glue/src/RustGlue.roc` declares six parameters
and documents the contract:

> The final `reuse` pointer is nullable. Non-null must be the callable data
> pointer whose inline capture begins at `capture`; it transfers one owned
> reference to the callee. The caller must not use or decref that ownership
> unit after the call. The callee consumes it exactly once, whether or not the
> result can reuse the allocation.

The compiler side is `LirProcSpec.erased_reuse_arg` in `src/lir/LIR.zig`, which
says every erased-callable ABI proc records its final argument there "regardless
of whether its result can reuse the allocation", and that ARC always consumes a
non-null transfer. So the callee unconditionally drops that argument. Nothing
supplies it, so it drops garbage.

The vendored file's mtime is 2026-08-04, which is misleading: its content is the
four-parameter form last seen upstream at `34fe463f45` (2026-06-25). A fifth
parameter appears in `1b435aaeae` (2026-07-11), `786cfa96ff` (2026-08-01) puts
the nullable `reuse` pointer in its current slot, and HEAD carries six. Trace
this with `git log -G`; `git log -S` reports only commits where the number of
occurrences of a string changed, so it misses every arity edit.

## The evidence

The generated proc saves `x4` on entry and decrefs it on the way out:

```
roc__proc_1dd  <+28>:  mov x20, x4
               <+32>:  mov x19, x1
               <+36>:  mov x21, x0                   ; = HOST+8 at the fault
               <+40>:  ldp q0, q1, [x2, #0x60]       ; reads 0x88 bytes from x2
               ...
               <+312>: mov x0, x20
               <+320>: bl  roc_llvm_rc_decref_35     ; faults, x0 = 0x88
```

The host writes four argument registers and never touches `x4`:

```
call_handler   <+636>: ldr x3, [sp, #0xa0]           ; capture
               <+640>: ldr x0, [sp, #0x130]          ; host
               <+644>: add x1, sp, #0x2c0            ; return slot
               <+652>: add x2, sp, #0x138            ; args
               <+656>: blr x8
```

`roc_llvm_rc_decref_35` opens with `cbz x0`, so it is null-safe, exactly as the
"may be null when the caller declines reuse" contract requires.

## Reproducing the fault, and proving the cause

Build the host in debug and one request kills it:

```bash
cargo build -p tower-platform-host
mkdir -p platform/targets/arm64mac
cp target/debug/libtower_platform_host.a platform/targets/arm64mac/libhost.a
"$HOME/.bin/roc" build --output=/tmp/srv examples/repro-keepalive/main.roc
PORT=8080 /tmp/srv &
curl -s http://127.0.0.1:8080/ping    # server dies; curl gets no response
```

A release host survives single requests and dies under sustained keep-alive
load, which is how this was first hit, via `examples/realworld` under `wrk`.
The host build mode only changes what garbage is sitting in `x4`.

The decisive experiment is the arity, not the crash. Widen
`RocErasedCallableFn` in `crates/host/src/roc_platform_abi.rs` and pass the
extra arguments at the two hand-written call sites, then rebuild the host:

| host | repro-keepalive | realworld |
|---|---|---|
| 4 params, as vendored | crash on request 1 | crash on request 1 |
| 5 params, pass `136usize as *mut u8` | **crash, identical fault** | not run |
| 5 params, pass `null_mut()` | 700 requests clean | not run |
| 6 params, as upstream spec | clean | 104 requests, no crash, register 201 |

Feeding 136 in deliberately reproduces the original fault exactly. That is the
proof the argument count is the cause rather than a correlate.

## Fixing it properly

Re-vendor the spec and regenerate:

```bash
cp ~/Repositories/roc/src/glue/src/RustGlue.roc vendor/RustGlue.roc
rm -f target/glue.fingerprint
just glue
```

Two things that regeneration will not do for you:

- **The hand-written call sites are not generated.** `crates/host/src/roc_call.rs`
  (`call_handler`) and `crates/host/src/effect.rs` invoke `callable_fn_ptr`
  directly and each needs the new arguments added by hand. `grep -rn
  callable_fn_ptr crates/` finds them.
- **The regenerated ABI file is much larger.** The vendored spec is 2641 lines
  against the current 3167, so expect churn well beyond this one signature.
  Generate into a scratch directory and diff first if anyone else is working in
  the tree.

## Why this took so long

The investigation went after the host's memory management and found nothing,
because there was nothing to find. Ruled out, all correctly, and all irrelevant:
the request arena (reproduces with the arena off), the warm-chunk pool
(zero-filling recycled chunks does not help), refcount atomicity (forcing every
refcount atomic does not help), heap use-after-free (a quarantining allocator
that poisons freed memory `0xFB` and fresh memory `0xAB` never fires a
double-free, and 136 is neither pattern), the profiler, the streaming and
encoder work, and the response cache.

That last result was the tell and was read backwards. The faulting value comes
from no tracked heap allocation because it was never a heap pointer. It is an
uninitialized register.

Two cheaper routes existed:

- **Count the argument registers.** Disassembling the host call site and the
  callee takes a few minutes and settles it. A decref of a small constant that
  equals a struct's size, in a proc called straight from a host `blr`, is an
  argument-count mismatch until proven otherwise.
- **Diff the glue spec.** `grep -n "RocErasedCallableFn = "` on
  `vendor/RustGlue.roc` and on `src/glue/src/RustGlue.roc` shows it immediately.

The same bug hit roc-solid on 2026-08-05 with the same signature and the same
`x4` mechanism. Re-pinning `~/.bin/RustGlue.roc` fixed that one and left this
repo exposed, because this repo vendors its own copy: the Justfile defaults
`rust_glue` to `vendor/RustGlue.roc`, not to `~/.bin`. Any repo that vendors a
spec has to be re-pinned on its own.

## What is not explained

The proc reads `x4` and the current spec declares six parameters, so there is a
sixth (`*mut *const c_void`) whose purpose was not chased down. Passing null for
it works, and passing nothing for it also happened to work in every case tested
here, but that is not evidence it is unused in general. Read the spec before
assuming a five-parameter host is sufficient.
