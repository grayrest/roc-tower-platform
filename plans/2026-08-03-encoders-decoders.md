# Encoders & decoders — coeffects as formats, `Res.json` as an encoder

Grill-me outcome, 2026-08-03. Exploratory branch. The compiler's Encoding
protocol (`parser_for` / `encoder_for` over a userland format nominal) is a
better fit for four things the platform currently hand-rolls: JSON body
decode errors, query/param extraction, response body serialization, and the
SQL boundary. Three goals, ranked: **(a) error quality** — name the offending
field and accumulate, without the upstream `lower.zig` work EX3 was blocked
on; **(b) inferred input records** — the handler's field accesses *are* the
extraction spec, which also closes the query half of the cache-variant DoS;
**(c) performance** — encode straight to bytes, decode straight off a cursor.

## Established facts (probed, not assumed)

- **A format is a userland nominal.** `roc/test/cli/ParserCountedFormat.roc`
  defines a whole format as `Counted := [Default].{ parse_str, parse_u64,
  parse_record_start, parse_record_field, skip_record_field, … }` with its
  OWN error union (`[Bad, ..]`), driven by `a.parser_for : Counted -> (State
  -> Try({ value : a, rest : State }, err))`. The format owns `State`.
- **`Builtin.Encoding.JsonEncoding` is NOT reachable from user code.** An
  earlier draft of this plan claimed it was, citing CIR.zig:192 — that is the
  compiler's INTERNAL builtin registry and the entry carries
  `.auto_import = false`. Upstream enforces the hiding on purpose:
  `test/cli/ParserHiddenEncodingTypesRejected.roc` declares
  `Encoding.JsonState` and `parallel_cli_runner.zig:1224` asserts `roc check`
  FAILS it with `MISSING NESTED TYPE`. The public `Encoding.Json` container
  (Builtin.roc:102–283) exposes scalars, `parse_null`, the list/tuple
  protocol and `to_str`/`parse` — but NOT `parse_record_start`,
  `parse_record_field`, `skip_record_field`, `invalid_value`, `rename_field`,
  or `parse_key_*`, i.e. exactly the record protocol an adapter would need.
  A delegating adapter is impossible; the platform format is a **vendored
  copy** of the `JsonEncoding` block (Builtin.roc:1478+).
- **The vendored copy is feasible; four primitives are not copyable.** Every
  `json_*` helper is ordinary Roc with a body (`json_u64_to_str = |v|
  U64.to_str(v)`, Builtin.roc:21351; `json_trim_start` :22493;
  `is_json_scalar_delimiter` :22514). But `u8_list_with_capacity` (:21655),
  `str_split_first_raw` (:22561), `str_get_utf8_byte_unsafe` (:22570) and
  `str_substring_unsafe` are declared with NO BODY under "Implemented by the
  compiler". Public replacements: `List.with_capacity : U64 -> List(item)`
  (:3885) — so the encoder's capacity-seeded byte buffer works in userland —
  and `Str.split_first` (:2424), the public wrapper over the raw split, which
  PRESERVES the zero-copy slices. No public equivalent exists for the
  byte-at-index / substring primitives, so the zero-copy scalar splitter
  (Builtin.roc:1440–1466) must be rebuilt on `Str.split_first`: slices
  survive, but it scans once per candidate delimiter instead of one byte pass.
- **`Json.to_str` IS the derived encoder** (Builtin.roc:234–239):
  `Shape.encoder_for(JsonEncoding.Default)` into `JsonEncodeState.{ output:
  u8_list_with_capacity(64) }`, then bytes→Str. `Json.to_utf8` does not exist
  upstream; `8cbb8d2` reverted the local patch that added it and took the 5%.
  `Res.json` can call `encoder_for` itself and return `encoded.output`.
- **Format methods are PURE.** `lower.zig` models effectfulness for derives —
  it carries both `.map` and `.map_effectful` result modes — but there is no
  `parser_for!` / `encoder_for!`. So a format cannot call the host, and a
  decoder over a live cursor (`Host.sql_col_i64!`) is upstream-blocked.
- **The demanded field set is available at runtime inside a format.**
  `parse_record_field` receives `FieldName.FieldNames(_shape)` with `iter` /
  `shortest_name` / `longest_name` (Builtin.roc:68–73); `HttpHeaderEncoding`
  uses the length prefilter at Builtin.roc:2165 and maps names through
  `rename_field` (underscores→dashes, Builtin.roc:2047).
- **`FieldNames` does NOT expose optionality** — the only method on `FieldName`
  is `name` (Builtin.roc:67–76; `index` lives in the hidden backing record and
  is rejected). `FieldNames` itself exposes `rename_fields`, `shortest_name`,
  `longest_name`, `iter`, `for_size`. So a format cannot distinguish
  absent-required from absent-optional, and synthesizing missing fields on
  `Done` is unsound (`Users.UserPatch` is five `Try(_, [Missing])` fields that
  must decode to `Err(Missing)`, not `Ok("")`).
- **Optional fields split into two things, only one of which is pending.** The
  PARSER side is on `origin/main` today — `e7543d9e65` "Derive optional parser
  fields from wildcard Try error rows" — and `ParserCountedFormat.roc` asserts
  a USERLAND format decoding `{ name : Try(Str, [Missing]) }` to
  `Ok({ name: Err(Missing) })`. The driver resolves optionality; the format
  never sees it. What is pending is the declared `name ?: Type` SYNTAX
  (`5875c6b141`, `af99de694d`, `badf0f2eed`), which lives only on
  `remotes/origin/jared/optional-records`, is NOT an ancestor of
  `origin/main`, and is ~3 weeks out per its implementer (2026-08-03).
- **`FieldNames.iter` yields ALPHABETICAL order, not declaration order**
  (probed: `{zebra, apple, mango}` → `["apple","mango","zebra"]`). Load-bearing
  for decision 14 if generated `SELECT` text is ever paired with positional
  row reads.
- **Format purity is not ENFORCED, it just breaks.** An effectful format
  method draws only an `EFFECTFUL FUNCTION NAME` warning; with a real host
  call the compiler PANICS (lower.zig:36816, "checked parser encoding registry
  was missing a scalar parser method") rather than erroring. Phase 8 should
  expect a panic, not a diagnostic.
- **Inference-driven decode already works**: `roc/test/cli/JsonParseInferred
  Record.roc` and `JsonParseInferredArticle.roc` call `Json.parse` with no
  annotation and reach `.title` / `.author` / `.views`.
- **The cache key contains the raw query verbatim** — cache.rs:4, *"everything
  knowable before the handler runs — (method, path+query, …)"*. The
  junk-variant eviction DoS that EX2 closed for bodies is still open on the
  query side, and it is the worse half: no POST body needed, and GETs are the
  only things that cache.
- **`query : Str` is a request-record field** (main.roc:29), violating the rule
  stated four lines above it — *"the request record carries only routing-pure
  data … the body and the Cookie header are NOT fields (D13/D14)"*. Any app can
  read it unobserved; `examples/e2e/main.roc:112` already does. Instrumenting
  the module-private `parse_pairs` would instrument only the platform's own
  readers and produce a trustworthy-LOOKING negative signal that isn't.
- **Static responses are never cached**, so there is no host-side query reader
  to audit: `storable` requires `CacheDeps` (middleware.rs:383), attached only
  on the Roc call path; `ServeFilesMount` (router.rs:143) never enters Roc.
- **sqlgen already emits positional reads** (DbGen.roc:57 —
  `Host.sql_col_i64!(handle, 0)`, no `Sql.Row`, no `List(SqlValue)`). The
  article-list query is the ONLY non-generated read, because its SQL is
  assembled at runtime from three optional filters (Db.roc:178, 362) — it is
  dynamic by construction, not by necessity.
- **`Res.Response` carries `effects : List(Res.Effect)`**, and `Must` holds
  `Box(Res.Err -> Res.Reply)` — a boxed closure no derive can conjure. Every
  write handler is shaped like `Articles.update` (Articles.roc:200–241): fold a
  `Res.fx(DbWrite(…))` builder, THEN `.json(200, …)`.
- **`Api.field_error` interpolates a DYNAMIC JSON key**
  (`{"errors":{"<field>":[…]}}`, Api.roc:16) — no encoder can produce it.

## Decisions

1. **Ranking**: (a) error quality is the deliverable, (b) inferred input
   records is load-bearing (see 8), (c) perf is real but partly upstream-gated.
   Pure-Roc work lands before any compiler patch.
2. **Decode is eager over the INFERRED row**, not lazy per access. Each
   `params.x` adds a field to the row; the whole row parses once. The field set
   is the union over the handler body, including branches never taken (probed,
   not assumed: a field read only in an untaken branch still yields
   `MissingRequiredField`). **Unannotated is the specification target because
   it is the strictly harder case — if it works without an annotation it works
   with one.** Annotation stays available per-site and SHOULD be used where an
   endpoint's accepted-parameter list is a public wire contract; that is where
   review flagged the real costs of inferring (a typo adds a required field and
   400s all traffic; numeric defaulting silently decides whether `?limit=-1` is
   an error; the accepted-parameter set is readable nowhere). Those are
   arguments for annotating specific routes, not for changing the mechanism.
3. **Input semantics**: required by default; optional is `Try(a, [Missing])`,
   which the use-site `match` pins (this is the only "wiring" left, and it sits
   next to the default it supplies). **Present-but-malformed REJECTS** — no
   lenient fallback, extending EX4's rule (`query_i64_or`) uniformly.
   Whole-record accumulate-then-reject. Accepted consequence:
   `/api/articles?limit=abc` changes from 200-with-clamp to 400.
4. **No lenient mode, ever.** A malformed value collapsing onto a default would
   make the canonical cache key unable to distinguish "asked for garbage" from
   "asked for the default", re-opening what 8 closes.
5. **Two input formats, plus the body adapter**:
   - `QueryEncoding := [Default]`, `State := { raw : Str, cursor : U64, field :
     Str }` — walks the RAW query, percent-decodes only demanded pairs (today
     `Req.get_query` decodes every pair to find one), `field` carries the
     current name so scalar parsers can report `InvalidField(name, expected)`.
   - `PairEncoding := [Exact]`, `State := { pairs : List({name,value}), index :
     U64, field : Str }` — over `req.params`. `Caseless` (+ `rename_field`
     underscores→dashes, the `HttpHeaderEncoding` convention) arrives WITH a
     real header caller; realworld reads one header and matches on
     `split_on(" ")` (Auth.roc:31), so the header decoder is speculative.
   - `JsonBodyEncoding := [Default]`, `State := { input : Str, field : Str,
     errors : List(…) }` — a **VENDORED COPY** of Builtin.roc's `JsonEncoding`
     block, with the four compiler-only primitives replaced as recorded in the
     facts (`List.with_capacity`, `Str.split_first`, splitter rebuilt). Owning
     the source is what buys the error tags, the accumulation, and (later)
     hash-during-parse. Cost, stated plainly: the platform now owns a JSON
     parser and can diverge from the builtin, and realworld's 154/154 becomes
     a conformance test of OUR parser.
6. **Cookies are NOT converted** (negative result, write it down). D13's precise
   Vary needs per-NAME read attribution; a record decoder demands bulk access,
   which over-approximates the variant spec to every cookie present and
   re-opens the junk-variant DoS. `Host.cookie!(ctx, name)` is the only cookie
   access and `grep -rn cookie examples/` returns nothing. Keep `Req.cookie`.
7. **Accumulation ceiling, accepted as temporary**: MISTYPES accumulate (the
   adapter records `InvalidField`, calls `JsonEncoding.skip_record_field` to
   consume the offending value, returns a fabricated scalar that never escapes
   because the resolver rejects on `rest.errors`); MISSES stay first-error,
   because `MissingRequiredField` comes from the generated driver and
   synthesis is unsound per the `FieldNames` fact above. Nested records thread
   the same `State`, so `{user:{email,username,password}}` accumulates all
   three. **The ceiling is the generated driver emitting one
   `MissingRequiredField` at a time — NOT the pending optional-field work.**
   `Try(a, [Missing])` already works in a userland format on today's main (see
   facts), so nothing about `?:` landing changes this. Lifting it needs either
   optionality exposed on `FieldName` or an accumulating driver; file that,
   don't wait on it. The payload is already `List(_)` and `Req.explain` already
   has a wildcard, so enrichment lands with zero app churn whenever it comes.
8. **Query canonicalization + drop-when-unread — with four corrections that
   adversarial review made non-negotiable.** After decode,
   `Hasher.hash_of(decoded, seed)` → `Host.set_query_key!(ctx, key)`, with a
   raw→canonical alias so lookups (which precede any decode) still hit; raw
   reads win, exactly as for bodies. But:
   - **The alias table must be SCOPED BY BASE IDENTITY and SEPARATE from the
     body table.** `alias_body_key(raw, canonical)` is a flat
     `HashMap<u64,u64>` (cache.rs:212, 262) with no method/path — body and
     query raw hashes would share one `u64` namespace. Under drop-when-unread
     every no-query route canonicalizes to the SAME sentinel, so the collision
     is guaranteed, not unlucky: `GET /api/tags?limit=5` registers an alias
     that makes `GET /api/articles?limit=5` hit the `limit=20` entry —
     20 articles served for a request that asked for 5, with no handler run.
   - **Drop-when-unread must be a STATIC ROUTE ATTRIBUTE, not learned from
     traffic.** Lookup happens at middleware.rs:199, before routing and before
     Roc, so a traffic-learned signal comes from a PRIOR request's read count —
     and coeffect short-circuiting (`Cofx.and_then`, a rejecting resolver, an
     auth gate before the query is forced) means read behaviour is not constant
     across requests. Decision 2's demanded field set is a static property;
     emit it as a route-table attribute at build time and consult it
     pre-handler.
   - **Canonicalization is fail-safe; dropping is fail-OPEN.** Raw-wins
     degrades to the conservative key; erasure degrades to a wrong response
     served across clients. Ship the two as separate commits and put dropping
     behind config so it can be turned off without a rebuild.
   - **A residual reader survives**: `headers` stays on the request record
     (router.rs:188), and header reads are attributed nowhere. Behind a
     URI-rewriting proxy (`x-forwarded-uri`, `x-original-url`), a handler can
     read the query with zero attribution. Either probe it or document that
     dropping is unsound behind such a proxy.
9. **`query` comes off the request record** → `Host.request_query!(ctx) => Str`,
   which reports a raw read. This is the D13/D14 move a third time and is what
   makes 8's negative signal true by construction. `params` STAYS on the record
   — path params are derived from the route pattern, already in `path`, so they
   carry no variance the key lacks; keeping them also keeps `PairEncoding` pure
   and expect-testable, and preserves `Req.param("slug")->Cofx.and_then(…)`.
10. **Structure every decoder as pure core + thin effectful resolver**, mirroring
    the body side (`body_str`/`json`/`form` are expect-tested; the coeffects
    are e2e-tested). Once query is host-mediated, `expect` can't drive the ~10
    existing `{ query: "a=1" }` literals; the pure core keeps that coverage.
11. **Delete the scalar factories**, don't deprecate: `query`, `query_or`,
    `query_i64`, `query_i64_or`, `param_i64`. Two paths to the cache key is
    precisely the raw-wins hazard. `Req.param(name)` survives (one path
    segment, no record to infer).
12. **Response: take C1, drop C2.** C1 = `Res.json` / `Res.Fx.json` call
    `encoder_for` directly and return the bytes, seeding capacity above the
    builtin's hardcoded 64. C2 = `Into<Response>` **does not port** — the
    underivable part of a response is the effect list (fact above), so a derive
    could serve only the read handlers, which are already one line
    (`Articles.get`). The error half already exists as `Res.mapped(handler,
    render)` and is BETTER than a derive because `Api.field_error` needs a
    dynamic JSON key. **Caveat from review, and it is a fair hit:** the
    effect-list argument rules out a DERIVE over the response type, not a
    SUCCESS-SIDE LIFT at the registration boundary. A handler returning
    `[Json(U16, a), Text(U16, Str), Resp(Res.Response)]`, collapsed by the
    route wrapper, gets the ergonomic win, composes with effects (the `Resp`
    arm carries them), and derives nothing over the effect list. That is a
    real design and this decision should not be read as closing it — it is
    deferred as cosmetic, not refuted.
13. **SQL splits by purity.** Param encoding (`encoder_for` → `List(
    Host.SqlValue)`) is pure and lands now, but only delivers "no chance of
    getting it wrong" paired with NAMED binding (`DbWrite` params become
    `List({name, value})`, host binds `:slug`) — positional `?` leaves the
    ordering bug intact. Row decoding needs `parser_for!` upstream.
14. **`SELECT ${fields}` is real but narrow.** A `FieldsEncoding` whose first
    `parse_record_field` reads `FieldNames.iter` and returns
    `Err(CollectedFields(names))` yields the column list with no value ever
    constructed — pure, no upstream, `rename_field` gives name mapping free.
    Reach is FLAT SINGLE-TABLE projections only: 2 of 6 generated queries
    (`tags_q`, `article_meta_q`). The other four are three-table joins with
    correlated-subquery columns (DbGen.roc:8) that a flat record cannot
    describe. **Do not grow a per-query column-mapping layer** — that rebuilds
    sqlgen at runtime, invisible to the VDBE analyzer and therefore to the
    cache's read-set derivation. Cost to measure: the join is recomputed per
    request (no compile-time eval, and the point is that the type is inferred
    at the call site, so it can't hoist to a named constant).
15. **The article-list regression is fixed by a FIXED-COLUMN-SET sqlgen
    directive — not by enumerating statements, and not by the decoder.**
    An earlier draft justified enumerating the filter combinations on the
    grounds that it "restores compile-time VDBE read-set derivation" and
    "retires the hand-declared `article_tags` dep". **Both were false for this
    tree** (probed 2026-08-03): read-set derivation is entirely RUNTIME via the
    authorizer shadow — `cursor.rs:200-206` sends every `sql_begin!` through
    `shadow::analyze`, memoized per SQL text (`MEMO_CAP` 4096), and SQLite
    compiles trigger programs at prepare time so cascades are expanded with no
    trigger-body parsing (shadow.rs:1-19); generated and dynamic SQL are
    treated identically. And no hand-declared dep exists — `article_tags`
    appears in the host only in test fixtures (cache.rs:583, effect.rs:449).
    So enumeration's only benefit was positional reads, at a cost of ~16-26
    generated statements with differing params record types and 8-arm matches
    at two call sites.
    The cheap fix gets the whole measured win: `article_summary_select` and
    `article_summary_select_anon` (Db.roc:296, 310) have IDENTICAL FIXED
    12-column projections — only the WHERE clause and the count spelling vary
    (Db.roc:176-193). A sqlgen directive emitting a positional row reader for
    a declared column set, DECOUPLED from statement text, deletes
    `article_summary_from_row`'s by-name reads (Db.roc:321-345) with zero
    combinatorial expansion.
16b. **This also removes phase 8's motivation.** With the fixed-column-set
    reader covering the only dynamic read in the tree, a `parser_for!` row
    decoder has no in-tree consumer. File the upstream ask with the sketch;
    do not patch the compiler for a feature with no user. Note also that a
    name-driven row decoder would reintroduce by-name column lookup — the very
    thing `notes/2026-07-15-typed-vs-json.md` identifies as the cause of the
    list regression — and that cursor-finish-on-reject has no owner
    (`Sql.roc:100-106` guarantees the handle never escapes today).
16. **`parser_for!` is filed with the row-decoder sketch as its motivating
    case** and patched locally only in the last phase. A second unpushed roc
    branch (the typed-sql fix `b697ad5001` is still local) turns "needs a
    custom roc" into the baseline, so it must not gate anything else.
17. **No content-type-dispatching `BodyEncoding` yet.** Both encodings are
    expressible (one nominal with a union `State`, paying a `match` per scalar
    forever; or N nominals with resolver-level dispatch, where the `where`
    clause becomes the union of all formats at every call site). The second is
    the right seam and it is a RESOLVER, so deferring costs nothing. Keep the
    name `JsonBodyEncoding` specific; add `Req.body!` when a second format
    exists.

## Phase −1 — cache fixes, on `main`, BEFORE the branch

Adversarial review of the cache found four live defects in shipped code. None
depend on this branch; two are exploitable today; and decision 8 amplifies all
four. Land them first, separately.

- **Unseeded hashers.** `variant_hash` (cache.rs:106), `body_hash_of` (:218)
  and `etag_for` (:226) all use `DefaultHasher::new()` — fixed-key SipHash,
  identical in every process, craftable offline (~2³² for a 64-bit collision).
  Only the CANONICAL key is seeded (effects.rs:592). On a cookie-varying route
  this is cross-user response serving by the D13 design's own terms. Seed all
  three from a per-process `RandomState`; domain-separate body vs query.
- **`order` grows without bound.** `inner.order.push_back(…)` is unconditional
  but the drain is `while inner.len > max_entries` (cache.rs:350–353). A
  re-store of an existing `(base, variant)` leaves `len` unchanged, so the
  loop never fires. Canonicalization makes that the STEADY STATE — every junk
  query misses and re-stores under one canonical key. Push only on newly
  created variants, or drain dead pairs.
- **Alias table unscoped** (cache.rs:212, 262) — see decision 8.
- **Alias table cleared WHOLESALE at 4× capacity** (cache.rs:252–258). Sharing
  it with queries means a junk-query flood wipes every route's body aliases.
  LRU, not clear-all.

Also record: the DoS being closed is a MEMORY bound, not a CPU one. Every
distinct junk query still misses, runs the handler and the DB read, and takes
the store mutex. The 07-17 plan scoped CPU out because body floods are bounded
by `MaxBodyBytes`/timeout — **that argument does not transfer to queries**,
which need no body and target the cheapest request shape.

## Phases (agreed order — pure Roc first)

0. **Spike, and it must be the real thing.** In `examples/repro/`: a format
   defined in `platform/`, consumed from an app, decoding an INFERRED record
   that includes (a) a required field, (b) an optional `Try(_, [Missing])`
   field that is ABSENT, and (c) a mistyped field that accumulates. A
   two-field toy passes while missing everything that matters — the earlier
   version of this phase would have passed without touching the JSON
   vendoring at all. Note: a parser-side version of this spike has already
   been run successfully against the current compiler (platform-defined
   format, app-side inferred record, `where [a.parser_for : …]`), so the
   platform-boundary risk is largely retired; what this phase gates is the
   optional/accumulation half.
1. **Vendor `JsonBodyEncoding`'s encode half + `Res.json` / `Res.Fx.json`**
   (5, 12). Own `EncodeState := { output : List(U8) }` seeded through
   `List.with_capacity`, return the bytes — this is how the 5% reverted in
   `8cbb8d2` comes back WITHOUT `Json.to_utf8` upstream. Bench gate.
2. **Vendor the decode half + mistype accumulation** (5, 7). Splitter rebuilt
   on `Str.split_first`. `Req.explain` grows a branch; `InvalidBody(List(_))`
   widens with zero app churn. Gate: realworld 154/154 both engines is now a
   conformance test of the vendored parser — run it before anything else
   depends on it.
3. **`QueryEncoding` + `PairEncoding.Exact`, pure cores** (5, 10, 11). No ABI
   change yet; scalar factories deleted; `Req.param` survives.
4. **ABI: `query` off the record** (8, 9). `Host.request_query!`,
   `Host.set_query_key!`, alias table, drop-when-unread, glue regen + committed
   ABI (precedent `0cfe4e3`), `examples/e2e/main.roc:112` migrated.
5. **`FieldsEncoding` + `SELECT ${fields}`** (14), used where it derives.
6. **sqlgen fixed-column-set row reader** (15) — a directive that emits a
   positional reader for a declared column set, decoupled from statement text.
   Deletes `Db.article_summary_from_row`. No statement enumeration.
7. **SQL param encoder + named `DbWrite` binding** (13) — host + `Effect.roc` +
   ABI. **Check turso first**: it binds positionally only
   (`turso_db.rs:137-138`, `305-306`), so named binding must be translated for
   that backend or the 154/154-both-engines gate fails at the END of this
   phase. Consider the cheaper alternative first — routing the remaining
   hand-written `DbWrite` SQL through sqlgen (`@sql write`), which already
   resolves `:name` → position at codegen and kills the ordering bug with zero
   ABI and zero host change.
8. **`parser_for!` patch + row decoder** (13, 16) — timeboxed, named roc
   branch, `ROC=` documented in the Justfile header.

## Testing

- Phase 0 is its own gate: it either builds and runs or the branch re-plans.
- Pure formats get `expect` coverage in-module (decision 10): decode cores over
  `Str` / `List({name,value})` literals, `FieldsEncoding` over a named type.
  One-line `##` doc comment before every `expect`.
- connect-check grows a case asserting a mistyped field's NAME appears in the
  400 body, and that two mistyped fields both appear.
- Phase 4 probe, mirroring EX2's: `?limit=20&utm=x` and `?limit=20` share one
  stored entry; `/api/tags?anything=here` mints exactly one entry total; a
  route reading `Req.query_pairs` keys on the raw string. **Plus, from review:**
  a cross-route probe (`GET /api/tags?limit=5` must not make
  `GET /api/articles?limit=5` hit the `limit=20` entry); MISS-RATE and
  alias-slot growth, not just entry count — the entry-count assertion passes
  while the miss rate is 100%; and a route reading `x-forwarded-uri` is not
  silently collapsed.
- **Bench gate on phases 3 and 4**, which the first draft omitted. They add a
  Roc-side hash, a host call and an alias lookup to the hottest cached path,
  and the closest prior art in this repo (bypassed traffic must skip
  attribution or −20%) says that is exactly where regressions live.
- Four apps declare `query : Str`, not one — `connect/main.roc:30`,
  `e2e/main.roc:17`, `hello/main.roc:16`, `ctxmemo/main.roc:16` — plus the one
  real reader at `e2e/main.roc:112`. Phase 4 touches all five.
- realworld 154/154 on BOTH engines at every phase; `/api/articles?limit=abc`
  moving 200→400 is a deliberate, documented break (decision 3) — confirm
  whether the official Hurl suite asserts it before phase 3 lands.
- Bench gate on phases 1, 6, 7 (`Json.to_str` is 63% of list-endpoint CPU;
  phase 5's per-request field join is a regression risk, not a win).
