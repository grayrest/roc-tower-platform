# Cursor streaming: where the effects and copies actually come from

**Status: local design exploration on the `encoders-decoders` branch. NOT
filed, proposed, or submitted anywhere.** No roc issue, no PR, nothing upstream.
The last section states what is left to ask for, which is much less than this
note originally assumed.

This supersedes three earlier framings. The first wanted an effectful
`parser_for!` upstream. The second replaced that with a single linear `Row`
feature and claimed every effect and every copy traced back to its absence. The
third priced that feature honestly and found it was really two features. This one
concludes that **neither is needed**: both halves have sound host-side
substitutes, and the compiler owes this design one small thing that is not a
language feature at all.

The through-line of all four framings is that each one looked for the fix on the
wrong side of the FFI boundary.

## What works today, with no language change

Cursor cell reads can be typed pure, and the pure codec derives compose over an
effectful `step!` loop with no compiler support.

Verified 2026-08-31 against `~/.bin/roc` (`release-fast-b9fea521`) on a scratch
copy of the platform at `scratchpad/purehost/` (session 4b14272c). The edit was
three lines in the platform plus the app's call sites:

```
platform/Host.roc:74   sql_col_str! : U64, U64 => Str
                    -> sql_col_str  : U64, U64 -> Str
platform/Sql.roc:182   Host.sql_col_str!(handle, index)  -> Host.sql_col_str(...)
platform/main.roc:94   "roc_sql_col_str": Host.sql_col_str!, -> Host.sql_col_str,
examples/realworld/Db.roc (8 sites) and DbGen.roc (5), same rename
```

Results:

```
roc check platform/main.roc              0 errors, 1 warning
roc test  platform/main.roc              all 127 passed in 184.3ms
roc build examples/realworld/main.roc    0 errors, 1 warning, 10.7s
./main + POST /api/users, POST /api/articles, GET /api/articles
                                         correct JSON off the streamed list path
```

The single warning is at `Host.roc:74`:

```
── ● effectful function name ─ platform/Host.roc:74:5
This function performs an effect, so its name must end in `!`.
```

Hosted declarations are treated as effectful by that check unconditionally, so
declaring one pure trips it. It is a warning, not an error, and nothing
downstream cares: `src/lir/hosted.zig` records only `{ symbol, dispatch_index }`
for a hosted proc, so purity has no bearing on the calling convention. The
symbol is still wired and the binary still serves correct data.

### Why the naive version is unsound, and why it does not currently bite

As written above, typing the read pure is a lie. `sql_col_str(handle, i)` returns
different bytes at different cursor positions from identical arguments, so it is
not a function. It would bite if the compiler ever moved or shared a call on the
strength of its purity.

Nothing does that today. There is no CSE, no GVN, and no loop-invariant code
motion over calls anywhere in `src/lir/` (`range_prove.zig` and
`loop_append_promote.zig` use loop-invariant *facts* for bounds checks, not code
motion). And `MonoLlvmCodeGen.zig` emits zero `readnone` or `memory(none)`
attributes, so LLVM has no basis to hoist a roc call either.

That is an accident of the current optimizer, not a guarantee. The fix is not to
rely on it, and not to reach for a language feature either. It is to change the
argument so the function is genuinely a function. See "The host-only sound
variant" below.

## What the streaming work established

`Db.stream_list!` (`1bcc64d`) bypasses materialization: it reads each cursor cell
and writes it straight into the JSON buffer, so the 20 `ArticleSummary` records
(and their ~200 heap strings) are never built. Measured on the authenticated
(cache-bypassed) list: roc_side **76.6µs to 43.5µs (-43%)**, handler 100 to 68µs.

It is hand-written for one row shape, 24 threaded lines. The question this note
answers is what support would make that fall out of a *type* instead of being
hand-written per shape, and how cheap that can get.

## None of this is inherently effectful

Parsing, encoding, and transcoding are pure data transformations, bytes to value
to bytes. The `=>` appears only where a transformation touches a live HOST
RESOURCE, and there are exactly two such edges:

- the **SQLite cursor** on the read side, and
- a **socket or chunk writer** on the write side (an effectful encode sink).

The logic *between* those edges is pure. The evidence is already in the branch:
`JsonDec`, `QueryDec`, and `JsonEnc` are all pure and pass `roc test` as pure
expects. The `!` on `Req.json_body!` and `Req.query_record!` is the host FETCH
(`Host.request_body!`) and REPORT (`Host.set_query_key!`) around a pure
`JsonDec.parse`; the request body is already a `List(U8)` in memory, so decoding
it needs no effect at all. The pure derives fully cover every in-memory case.

## The cursor is the sole effectful source, and why

- `sql_begin!` and `sql_step!` are genuinely effectful: they run the VDBE, do
  I/O, and ADVANCE the cursor position.
- `sql_col_str!(handle, i)` is *morally pure*. `roc_sql_col_str` does
  `with_row(handle, |row| row.get_ref(i))`, a deterministic read of the CURRENT
  row's cell. Same position plus same index gives the same bytes.
- Incremental reads of an INTERNAL value are pure (`List.get`); of an EXTERNAL
  resource, effectful. The cursor's effectfulness is the price of being lazy
  against an external source, which is the only way to avoid materializing the
  whole result set.

Today the cursor is a bare `U64` handle:

```
sql_step!    : U64 => Try(Bool, Str)
sql_col_str! : U64, U64 => Str
```

The handle names the cursor but not the position, which is the entire problem.
Position discipline is by convention, and nothing stops a read after a step.

## The host-only sound variant

### Rust is not tracking these lifetimes today

The obvious thought is to recruit Rust's borrow checker, since the host is Rust
and SQLite's cell validity is exactly a lifetime. That is not available as a
drop-in, because the host has already erased those lifetimes on purpose to get a
handle-based API. From `crates/host/src/cursor.rs`:

```rust
let rows: *mut Rows<'static> = Box::into_raw(...);
cursor.current: *const Row<'static>
fn with_row<R>(handle: u64, f: impl FnOnce(&Row<'static>) -> R) -> Option<R>
```

`Row<'static>` behind a raw pointer leaves the borrow checker nothing to check.
Reintroducing real lifetimes would mean inverting control so the host drives the
row loop and calls back into Roc, and even then Rust would only bound the
callback body, never a `u64` that Roc kept afterwards. Inversion also fights the
pure-derive composition, which wants to drive the loop itself. Not worth it.

The Rust glue redesign on `claude/grill-me-rust-glue-e14b4f` does not supply this
either. Its lifetimes are `ElementView<'a>`, a borrowed view into a Roc tag union
**the host already owns**, which is host-side read ergonomics for Roc data. That
is the opposite direction from binding SQLite cell lifetimes into Roc. The branch
is relevant here, but for its ownership core rather than its lifetimes; see the
sink rung below.

### Generation-tagged row tokens

The fix is not to bound the lifetime. It is to put the position in the argument.
Have `step!` mint a globally unique token per row instead of handing back the
cursor handle:

```
step!   : Cursor => Try([Row(RowTok), Done(Cursor)], err)
col_str : RowTok, U64 -> Str
```

`RowTok` is a `u64` drawn from a process-global monotonic counter. The host keeps
the current generation on the `Cursor`, bumps it on every step, and `with_row`
takes a token, compares, and hard-errors on a mismatch instead of silently
serving the current row.

### Why this makes the purity claim true

This is not a tidier version of the discipline argument. It removes the lie.

With a globally unique token, `col_str(tok, i)` maps each argument pair to
exactly one answer for the life of the process, because the token identifies one
specific step event and nothing else will ever carry it. Two reads at different
rows now differ in their arguments, so there is nothing for a CSE pass to merge.
The token is loop-carried and data-dependent on `step!`, so no pass can hoist a
read out of the loop; that is SSA dataflow, not an optimizer courtesy.

So the soundness argument stops being "no such pass exists yet" and becomes
"there is no wrong answer available". That is the difference between rung 0 and
rung H1 below, and it is the whole point.

A partial version of this already exists. `roc_sql_step` nulls `cursor.current`
on `Ok(None)` and on error, so a read after Done is caught and returns empty.
What is not caught is a read after an *advance*, which silently returns the new
row's cell. The generation counter closes exactly that gap.

### What it does not give, and one residual hazard

Use-after-step becomes a runtime error, not a type error. That is a real
downgrade from a linear `Row` and should not be glossed.

The residual hazard runs the other way. Because `col_str` is pure, a future pass
could in principle sink a read past a `step!`, and the generation check would then
fire on a correctly written program: a spurious crash. Take it anyway. Today that
same motion returns the wrong row's data with no signal at all, and loud beats
silent. A crash is a bug report; a wrong row is a support ticket six months later.

## What a linear Row would add on top

```
step!   : Cursor => Try([Row(Cursor), Done(Cursor)], err)
col_str : Row, U64 -> Str
```

A use-once `Row` would upgrade the token design's runtime error to a compile-time
error, and would be the lifetime bound a borrowed `RocStr` needs.

That is now the entire delta. It is worth having and it is not worth blocking on,
which is a very different conclusion from where this note started. The earlier
claim that "every effect and every copy traces back to its absence" was wrong
twice over: the effects were expressible all along, and the copy has a host-side
route.

### The three capabilities this replaces

The original framing wanted three effectful derives. For the record:

1. `parser_for!`, an effectful SOURCE (decode from a cursor).
2. `encoder_for!`, an effectful SINK. This does NOT bypass materialization: it
   still consumes a materialized value. It makes the sink effectful, not the
   source.
3. `transcode!`, `parser_for!` welded to `encoder_for` at the field level so the
   intermediate value is never allocated. This is what `stream_list!` is.

All three are superseded. Pure derives over an effectful `step!` loop cover the
same ground and re-mark nothing pure as effectful.

## The cost ladder

Host-side rungs are `H`, compiler-side rungs are numbered.

| rung | what | side | copies | cost |
|---|---|---|---|---|
| 0 | pure-typed host reads | platform | 2 | **zero**, verified working, unsound |
| H1 | generation-tagged row tokens | host | 2 | **~60 lines Rust**, sound |
| H2 | host-side escaped-cell sink | host + glue | **1** | small, needs the glue redesign |
| 1 | let a host read opt out of the `!`-name warning | compiler | 2 | one file plus snapshots |
| 2a | uniqueness as a type-system axis | compiler | 2 | very large, declined |
| 2b | use-once checker for one nominal | compiler | 2 | ~2 weeks |
| 3 | borrowed-view `RocStr` | compiler + ABI | **1** | large, plus an ABI break |

**H1 and H2 together deliver everything 2b and 3 deliver**, minus compile-time
detection and minus app-defined leaf encoding. That is the finding that reorders
this whole document.

### Rung H1: generation-tagged row tokens

Add `generation: u64` to `Cursor`, bump it in `roc_sql_step`, change the row
token to carry it, and check it in `with_row`. Roughly 30 lines in `cursor.rs`
and the same again in `turso_db.rs`, plus the `Host.roc` signature change and the
app call sites. No compiler change. This is the recommended next step.

### Rung H2: a host-side escaped-cell sink

A host function that appends the escaped cell straight from SQLite's bytes into
the Roc output buffer:

```
sql_col_str_json_into! : List(U8), RowTok, U64 => List(U8)
```

One copy. Sound without any lifetime machinery, because the host never hands Roc
a pointer it does not own. What it gives up is app-defined *leaf* encoding.
Structure, field order, and derivation stay app-driven, so this is much narrower
than the "transcode host-side in Rust" option rejected earlier, which gave up the
whole shape.

**This is where the glue redesign matters.** The generated ABI today
(`crates/host/src/roc_platform_abi.rs`) has `realloc`, `is_unique`, `allocate`,
and `from_slice`, but no safe grow path, so writing this function now means
hand-rolled pointer arithmetic reaching behind glue's back. Plan items 25 to 28
(`RocList<T: RocElement>`, `Drop`, `Clone`), 43 (`RocList::from_iter`, allocating
`try_*` variants), and 44 (`refcount()`, `element_stride()`,
`is_seamless_slice()`) turn it into supported API. Item 44's own justification is
this problem verbatim: hosts reach behind glue because glue's beliefs are not
checkable, so make it a supported operation instead.

Not yet spiked. The open question is the realloc path when the buffer is shared.

### Rung 1: silence the warning honestly

The `effectful function name` check fires on any hosted declaration typed with a
pure arrow. If pure host reads are a supported pattern rather than a hack, the
check needs a way to say "this one is deliberate". One file plus the snapshot
corpus.

Note that the check is doing real work and should NOT simply be removed. Under
rung H1 the purity claim is true, but a platform that types a read pure *without*
adopting position-carrying tokens is making a false claim, and that is precisely
what the warning catches. The ask is for deliberate purity to be expressible, not
for the check to go away.

### Rung 2a: uniqueness as a type-system axis

Purity is the existing precedent for a discipline tracked in the type system, via
`fn_pure` / `fn_effectful` / `fn_unbound` on `Content`. It is threaded through 23
files and roughly 420 sites:

```
src/check/Check.zig            178
src/check/unify.zig             70
src/check/exhaustive.zig        35
src/types/types.zig             18
src/check/snapshot.zig          16
src/check/snapshot/diff.zig     14
src/check/checked_artifact.zig  12
src/types/TypeWriter.zig        11
...plus copy_import, instantiate, store, docs/extract, lsp/syntax, occurs
```

Uniqueness is strictly worse than purity to add, because it also constrains
closure capture and storage in data structures. It changes how every signature
prints, so the whole snapshot corpus churns, and it touches docs rendering and
the LSP. Declined.

### Rung 2b: a use-once checker for one nominal

A post-check pass over typed CIR that finds bindings whose type is a designated
nominal and enforces three rules: used exactly once, never captured by a lambda,
never stored in a record or list.

Existing scaffolding: `src/check/checked_traverse.zig` (703 lines) for the
traversal, `src/check/exhaustive.zig` (4535 lines) as the precedent for a
post-check analysis with real diagnostics, `src/check/problem/types.zig` (614
lines) for a new problem variant.

Estimate 600 to 1200 lines plus diagnostics and snapshots, roughly two focused
weeks. The hard parts are join points, where match and if arms have to agree on
what got consumed, and writing errors worth reading.

Under H1 this buys a compile-time error in place of a runtime one. That is a
quality-of-life improvement, not a blocker, and it should be priced that way.

### Rung 3: a borrowed-view RocStr

`RocStr` has three forms, none of which can point at foreign bytes: **small** (23
bytes or fewer, inline), **large owned** (refcounted Roc heap allocation with the
refcount word before the bytes), and **seamless slice** (points inside another
Roc allocation, sharing its refcount). All three assume Roc owns or manages the
memory, so `RocStr::from_slice` copies.

Two false shortcuts, both checked and both rejected:

- **Reuse the static-data sentinel.** `REFCOUNT_STATIC_DATA = 0`
  (`src/builtins/utils.zig:284`) already marks whole-program-lifetime data that
  is never freed, and string literals use it. But it requires a real refcount
  word immediately before the bytes, which `static_data.zig:1306`
  (`staticDataPtrOffset`) reserves at emit time. SQLite's cell bytes live inside
  a page buffer and have no such word.
- **Use capacity 0 as an untagged non-owning form.** `decref` returns early on
  `data_bytes == 0`, and `refcount()` returns 1 for a capacity-0 non-seamless
  string without dereferencing anything. But returning 1 means *unique*, so
  `canReuseAllocation` (`!isSmallStr() and !isSeamlessSlice() and
  isExclusive()`) says yes and a mutating `Str` operation would write into
  SQLite's page buffer. Silent corruption, not a crash.

A real fourth form needs a second tag bit in `capacity_or_alloc_ptr`, moving
`capacity_shift` from 1 to 2. Touch points:

```
src/builtins/str.zig        39 capacity_or_alloc_ptr sites, 18 uniqueness sites
src/builtins/list.zig       shared capacity/slice encoding
src/static_data.zig         literal emission
src/glue/src/RustGlue.roc   14 sites
src/glue/src/ZigGlue.roc    12 sites
src/backend/llvm/debugger/roc_gdb.py
LLVM, dev, and wasm backends
```

The sharp edge is `incref`. `getAllocationPtr` returns `bytes` for a non-seamless
string, so an incref on a borrowed view writes to `bytes - 1`, which is foreign
memory. Every incref path needs the tag check, and getting one wrong corrupts
someone else's page cache rather than failing cleanly.

It changes the Roc/host string representation, so every platform's glue must
agree. It cannot land as a local patch. And the payoff is narrow: cells of 23
bytes or fewer are already inline, and any cell needing JSON escaping still
copies. Only long, escape-free cells benefit, which in the realworld list means
description and body.

**H2 reaches the same copy count with none of this.** Rung 3 is retained here as
the record of an investigated and rejected route, not as a proposal.

## Copy accounting

Today, per string cell, there are two copies:

```
SQLite cell bytes -> RocStr        (sql_col_str!,  copy #1)
RocStr bytes -> output buffer      (str_into, escaping, copy #2)
```

| design | copies | needs |
|---|---|---|
| effectful `transcode!` (abandoned) | 2 | effectful codec derive |
| pure derives + effectful `step!` | 2 | **nothing, works today**, unsound |
| the same, made sound | 2 | **H1, host only** |
| the same, sound at compile time | 2 | H1 plus 2b |
| pure derives + borrowed cell views | **1** | 2b plus 3, ABI break |
| pure derives + host-side sink | **1** | **H1 plus H2, host only** |

Purity is copy-neutral. Effect tracking and memory movement are independent axes,
and making reads pure by snapshotting the row does not add a copy either: it
batches copy #1 into one transient row value, which is not the 20-record
materialization `stream_list!` killed.

## Measure before building the sink

H2 is only worth its cost if the second copy is a real share of the remaining
time. Before building it:

- What fraction of `stream_list!`'s 43.5µs is spent copying string cells at all.
- Of that, what fraction is cells longer than 23 bytes (below which the copy is
  inline and cheap) AND free of characters needing JSON escaping (above which the
  copy is unavoidable). For the article list that is description and body only.

If the answer is a couple of microseconds, H1 alone is the whole project.

H1 does not need this gate. It is a correctness fix at negligible cost, and a
generation compare per read is a u64 comparison against a value already in cache.

## Caveat that keeps the pure read sound

SQLite's column accessors have an internal type-conversion cache: `column_text`
then `column_bytes` can differ. "Pure" is sound only with disciplined fixed-type
accessors, `col_str` for TEXT and `col_i64` for INTEGER, never mixing accessors
on one column. The typed path already does exactly this.

This holds at every rung, including H1. A generation token pins the position, not
the accessor, so this stays a platform obligation that nothing here checks.

## Where the branch actually is

`stream_list!` is the SOURCE half of the abandoned `transcode!`, hand-written per
row shape. No records are materialized, but it still buffers the whole response
in a `List(U8)` and copies twice per cell.

The next step is H1 plus rung 0: add the generation counter and row token to the
host, introduce a `RowTok` in the platform, type the cell reads pure, and let the
existing pure derives compose over an effectful `step!` loop. That removes the
hand-written-per-shape problem, is sound, and touches no compiler code. It costs
one warning until rung 1 lands upstream, and the warning is honest about what it
is warning about, so living with it is fine.

## If ever taken upstream

The original ask is withdrawn. There is no longer a language feature blocking
this design, and filing one would misrepresent the problem.

What remains worth raising, smallest first:

**Ask 1, the only real one.** Let a hosted declaration be typed pure
deliberately, without tripping `effectful function name`. This is rung 1: one
file plus snapshots. The motivation is that a host read whose arguments determine
its answer, such as a cell read keyed by a position-carrying token, is genuinely
pure, and the language currently has no way to say so. Frame it as making
deliberate purity expressible, explicitly NOT as relaxing the check, because the
check correctly catches the naive version of exactly this pattern.

**Ask 2, an observation rather than a request.** A use-once discipline for a
designated nominal (rung 2b) would upgrade the host's runtime staleness error to
a compile-time one. Worth mentioning in passing if the conversation goes there.
It should be offered as a self-contained post-check analysis, and the
uniqueness-as-a-type-axis version (rung 2a) should be declined up front as far
more than the problem needs.

**Withdrawn: the borrowed `RocStr`.** A host-side escaped-cell sink (H2) reaches
the same one copy with no ABI change and no lifetime machinery. Proposing an ABI
break for a win that a host function delivers would not survive first contact,
and it should not.

`stream_list!` (`1bcc64d`, measured -43%) remains the concrete motivation for
Ask 1.
