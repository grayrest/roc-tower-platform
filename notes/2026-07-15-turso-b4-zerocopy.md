# B4: zero-copy borrow assessment — **the copy is the floor, and it's noise**

**Date:** 2026-07-15 · **Measured in** `crates/turso-spike`
(`measure_b4_copy_cost`, via `turso_core` directly) · macOS aarch64.

The B4 idea: Turso is a Rust-native engine, so maybe the typed cursor could
*borrow* column bytes straight into Roc values instead of copying them —
shrinking the per-cell marshalling the Phase 7 profile flagged (~30% of host CPU
on the response-construction path). Prototyping was gated on one measurement and
one soundness question. Both land firmly negative.

## 1. The copy cost is noise (measured)

`turso_core`'s `Row` already materializes owned `Value`s per row; `Row::get_value`
returns a borrowed `&Value` (no copy). rsapi's `row_value` then does an
*additional* `.to_owned()` — the current turso backend's redundant copy. Reading
the article-by-slug JSON document cell 100k times each way:

| access | ns/cell |
|--------|--------:|
| borrowed (`get_value().to_text()`) | 17.0 |
| owned (`get_value().to_owned()`, what rsapi does) | 87.6 |
| **redundant copy** | **70.6 ns (~0.07 µs)** |

That removable copy is **~0.01% of a ~950 µs turso `byslug` request**. Scaling
the doc from the 289-byte seed to a realistic multi-KB document, the copy is
memcpy-bound (multi-GB/s → ~1 µs for 10 KB) — still **under ~0.3%**. Removing
rsapi's redundant `to_owned` is a real single-copy win, but it buys nothing
measurable: DB engine time (B0's 8–17×) and HTTP/marshalling dominate.

## 2. True zero-copy into a Roc value is unsound

Even setting cost aside, borrowing turso's bytes *directly* into a Roc `Str`/
`List(U8)` cannot be made sound:

- **The refcount headroom word.** Roc's decref reads the refcount from the word
  *immediately before* the data pointer (`REFCOUNT_STATIC_DATA == 0` lives
  there). A RocStr/RocList pointing into turso's row buffer would have that word
  land on arbitrary buffer content — Roc would read a garbage refcount and free
  or corrupt it. Seamless slices don't rescue this: they require a *Roc*
  allocation with a real refcount slot as the backing, which turso's buffer is
  not.
- **Transient lifetime.** `Row::get_value` borrows the current row; the buffer is
  reused on the next `step()`. A typed handler that steps through rows (or reads
  several cells then steps) would be left with borrows into overwritten memory.

So any value that becomes visible to Roc code must live in Roc-shaped memory
(the request arena, which *does* have the headroom word and a request-scoped
lifetime) — i.e. exactly one copy. **The arena copy is the floor.**

## 3. The one escape hatch, and why it isn't worth it

The only value that never needs to be a Roc value is the `@sql json` response
*body*: it could, in principle, stream straight from a turso cell to hyper via
`Bytes`, keeping the statement alive with an owner (like A3's `RocBodyOwner`).
But:
- The body currently flows *through* Roc (the handler wraps the cell in `RawJson`
  and returns it), so bypassing that is an architecture change, not a borrow.
- Holding the statement/connection until the body is written pins it across
  client I/O — the exact hold-time bottleneck A3 deliberately rejected.
- And it would save a copy that §1 just measured at ~0.01–0.3% of a request.

## Verdict

**No-go on zero-copy / borrow work.** The copy the whole idea targets is noise,
and the sound version of it (arena copy) is already what the platform does. The
materialization follow-up B4 was meant to gate is likewise unmotivated *for this
reason* — though materialization remains a valid *read-throughput* lever on
rusqlite (the 2026-07-14 61–80% win), which is a different axis entirely.

The real lever for typed-on-turso is turso's own engine speed (8–17× vs
rusqlite); no host-side marshalling change moves that. This closes the arena +
turso plan: A shipped (perf-neutral), B0 NO-GO, B1/B2 built as the typed-on-turso
artifact (154/154, 1.75–4× end-to-end), B4 assessed — copy is the floor.
