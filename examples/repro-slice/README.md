# H3 spike: internal-iteration fold with borrowed-slice rows

Proves the mechanism the roc devs suggested for zero-copy host-owned buffers,
applied as a **general cursor fold**: match H2's one-copy JSON streaming while
dropping both of H2's hacks — the pure-typed host cell reads and the
behind-the-glue `RocList` growth.

## The mechanism

`Host.sql_fold_bytes!` (in `platform/Host.roc`) is an **internal iterator**:
effectful (`=>`), it drives the VDBE, and for each row hands a `List(SqlValue)`
to a **pure** (`->`) reducer that appends into a `List(U8)` accumulator via the
ordinary `JsonEnc` append path.

Each row's Text/Blob cells are **borrowed slices** built by `borrow.rs`
(`borrowed_str` / `borrowed_bytes`): a `RocStr`/`RocList` whose `bytes` pointer
aims straight at the live SQLite cell while its allocation pointer aims at a
shared, process-global, zero-filled 16-byte block. The refcount word there reads
`0` = `REFCOUNT_STATIC_DATA`, so both the host and the compiler's *own* generated
incref/decref treat the slice as immortal — read with native ops, never freed,
never mutated (`roc/src/lir/immortal_locals.zig`). Zero per-cell allocation.

Soundness is by **containment**, not a generation token: the borrowed row is
valid only for the duration of one reducer call and is invalidated by the next
step. Serializing copies the bytes out, so nothing borrowed escapes into the
accumulator. Internal iteration is what bounds that lifetime structurally.

## Why this drops both H1/H2 hacks

* The step is honestly effectful; the reducer is honestly pure — no host cell
  read is mistyped `->`, so no "effectful function name" warning to swallow.
* `out` grows through the supported Roc `List(U8)` append path, not by reaching
  behind the glue to grow a `RocList` in place.

## Result

`GET /rows` folds a seeded mixed-type table into a JSON array of arrays:

```
[[1,"jake","the \"real\" jake",42],[2,"bob",null,7],[3,"sy","tab\tand newline\nhere",0]]
```

Escaping the borrowed bytes (`\"`, `\t`, `\n`), integers as numbers, NULL as
`null` — all correct.

* **Correctness:** byte-correct JSON, exercising Integer / escaped Text / NULL.
* **Release host, sustained keep-alive load:** `wrk -t1 -c1` (325k requests on
  one reused connection — the exact keep-alive-segfault trigger) + `wrk -t4 -c32`
  (1.0M requests) — server survives, output still correct, log clean.
* **Debug host (the deterministic ABI-bug detector):** request 1 correct, and a
  71k keep-alive burst survives.

## Run

```
just build examples/repro-slice/main.roc
PORT=8231 ./main
curl -s http://127.0.0.1:8231/rows
```
