# Input extractors + canonical cache keys

> **STATUS 2026-07-17: ALL PHASES DONE** (EX3 assessed+deferred by design).
> EX1 `ff06b3f` tag rejections + Req.explain + connect reshape ·
> EX2 `e7ce107` canonical body keys (to_hash, alias table; upstream roc
> `59e30d7db6`+`5628dbfeef` expose Hasher.new/finish/hash_of) ·
> EX3 deferred — landing sites in notes/2026-07-17-json-parseerr-
> enrichment-assessment.md ·
> EX4 `797491c` scalar factories (parser_for IS format-generic —
> HttpHeaderEncoding precedent — derive not needed for scalars) ·
> EX5 `06f3e35` realworld/hello/e2e on bindN+Req.route, withN DELETED,
> sqlgen emits open unions.

Grill-me outcome, 2026-07-17. Two goals, one design: (a) kill the response
cache's remaining DoS vector (eviction pressure from junk variants of valid
messages), (b) make the framework own input boilerplate — hand-written wrap
coeffects like connect's `target_id!`/`new_book!` should not exist.

## Established facts (probed, not assumed)

- Error responses are ALREADY never cached: `storable = status.is_success()
  && ...` (crates/host/src/middleware.rs:364). Invalid-body floods mint zero
  entries today. The "skip errors" half of the original idea is shipped.
- The builtin JSON decode is LENIENT: `{"id":1,"junk":9}` decodes into
  `{ id : I64 }` and returns 200. Junk fields, whitespace, key order, and
  number spelling each mint a DISTINCT cache variant (raw body-hash keying,
  D14) for the SAME logical request → eviction-pressure DoS against a
  bounded cache (max_entries LRU). Memory is bounded; hit-rate is not.
- `Json.ParseErr : [MissingRequiredField(Str), InvalidJson(Str)]` — missing
  fields carry the NAME; a wrong-typed field is only the detail-free
  `InvalidJson("Invalid JSON")` (Builtin.roc:249). No error accumulation.
- Compiler-derived static-dispatch methods: `is_eq`, `to_hash`,
  `parser_for`, `encoder_for` (static_dispatch_registry.zig:795). `to_hash :
  self, Hasher -> Hasher` derives structurally for records — an in-memory
  walk, no serialization.
- A per-field extractor (`Req.body_field("id")`) is IMPOSSIBLE — the decode
  target's field name would have to come from a runtime string. Projection
  stays in the handler's argument type; extraction is whole-message.

## Decisions

1. **Threat model**: solve eviction pressure (duplicate variants of valid
   messages). CPU cost of uncached garbage is out of scope (bounded by
   MaxBodyBytes/timeout). Persisted queries out of scope (client contract).
2. **Built-in extractor coeffects reject with OPEN TAGS, not pre-rendered
   400 Responses**: `InvalidBody(List(Json.ParseErr))`, `MissingParam(name)`,
   `MissingQueryParam(name)`, `MissingHeader(name)`, `MissingCookie(name)`.
   They then bind directly in `Cofx.bindN` pipelines; the app's render maps
   them. `withN` + Response-rejection is deprecated across the board;
   connect is the API-feel lab, realworld migrates once the feel settles.
3. **Error payload**: structured list `InvalidBody([MissingRequiredField("foo"),
   InvalidField("bar", ...)])`; the app's render passes it to a platform
   formatter (`Req.explain : List(Json.ParseErr) -> Str`) for the human
   message. Ship NOW around today's ParseErr (singleton lists); upstream Json
   later adds `InvalidField(name, expected)` + record-decoder accumulation —
   payload enriches with zero platform-union or app changes. No mirror union
   (we control both sides of the Json boundary).
4. **Canonical body cache key — decode + structural hash, NO re-encode**:
   after a successful decode, `decoded.to_hash(hasher)` → U64 →
   `Host.set_body_key!(ctx, key)`. Junk fields can't enter (not in the
   value); whitespace/order/spelling died at parse. Seed the Hasher with a
   per-process random state (collisions not offline-craftable; cache only
   needs in-process stability; u64-collision exposure ≈ status quo raw hash).
5. **Composition rules (host, CacheEligible)**: any RAW body read
   (`body!`/`text_body!`/`form_body!`) wins — variant stays the raw hash.
   Multiple typed decodes: ordered fold of canonical tokens. Store side keys
   the entry on the canonical variant plus a BOUNDED raw-hash → canonical
   alias table (u64→u64, own LRU) — lookups happen before any decode exists,
   so they hit via the alias; junk raw-variants churn only alias slots,
   never stored bodies. Without the alias layer canonical keys reduce
   nothing (lookups could never hit them).
6. **Query/path params**: caching keeps the WHOLE raw query string as key,
   host-side (low-entropy; not worth canonicalizing — bypasses Roc
   entirely). The parsing/error path: spike a `parser_for`-based scalar
   encoding (`Req.query_as(name)` / `Req.param_as(name)`, target type from
   the handler slot, `InvalidQueryParam(name, expected)`); the old decode
   likely wasn't format-generic, but the derive is structured for it
   (Serde-aware authors) — fall back without regret to a closed scalar
   factory set (`query_i64`, ...) emitting the same tags. Vocabulary and
   render path identical either way; doesn't block anything.

## Phases (agreed order)

1. **Platform tag migration + connect reshape** (pure Roc): built-ins reject
   tags; add `Req.explain`; connect deletes `target_id!`/`new_book!`
   (`Cofx.bind1(Req.json_body!, delete_book)`), render grows
   `InvalidBody(errs) => Connect.error(InvalidArgument, Req.explain(errs))`.
2. **Host cache work** (closes the DoS thread): `Host.set_body_key!`, seeded
   hasher, raw-wins/ordered-fold, bounded alias table. Bench store-path;
   probe: whitespace/junk variants of one message share one stored entry.
3. **Upstream Json enrichment**: `InvalidField(name, expected)` +
   accumulation in the record decoder. Messages improve everywhere free.
4. **Typed param spike**: `parser_for` scalar encoding; fallback factories.
5. **Realworld migration + `withN` removal** once connect's feel is settled.

## Testing

- connect-check grows: junk-field/whitespace variant → same ETag AND same
  stored entry (hit on second variant); decode failure → not stored;
  missing-field message includes the field name.
- realworld 154/154 both engines at every phase (phase 5 especially).
- Bench gate on phase 2 (store-path cost; alias-table overhead).
