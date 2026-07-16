# Decision log: derived (automatic) response-cache dependencies

The full arc from "is the shipped cache's invalidation table-level?" through
three rounds of adversarial review to the final design: **derive cache
dependencies and invalidation sets from the SQLite authorizer against an
analysis-shadow connection** — automatic, column-scoped, no hand-declared
deps, no annotation DSL, no compiler API. Implementation plan:
`plans/2026-07-16-authorizer-derived-cache.md`.

## Framing (the load-bearing reframe)

The shipped response cache (`notes/2026-07-15-response-cache.md`) follows
normal framework patterns: the *author* declares what a response depends on
(`Res.cacheable(resp, tables)` + hand-maintained `Db.*_read_tables` lists).
The reframe that drove everything after: **because handlers are pure, response
caching here is memoization, not a framework bolt-on** — the platform sees
every read (coeffects) and every write (effects), so it should be able to
derive both the dependency set and the invalidation set itself. The objective
is the **platform**, not the example app: "Getting a free 2–14x speedup from
automatic caching for *every app* is worth regressing a hand-optimized
example." Correctly solving cache invalidation at the framework level has not
been done; "here's automatic caching" is a categorically stronger claim than
"this could enable caching."

That baseline choice decided several verdicts below: reviewer rounds 2 and 3
scored proposals against the already-hand-tuned RealWorld example (where
derivation is inert-to-regressive) instead of against the platform's real
baseline — an app whose author declared nothing and gets no caching at all.
Against the real baseline the same change is the difference between 0 and
~100k rps on anonymous hot paths.

## Decisions

**D1 — Memoization model (kept).** A coeffect is `(key, resolve!)`; identity
is a rolling scalar hash (U64 tokens, never strings — no alloc/serialize on
the hit path); deps accumulate miss-only; key face is read-free and may return
per-request `Uncacheable`. Round-1 review killed the *original* packaging
(4 reviewers: phantom rows, trigger-cascade staleness, `and_then` can't thread
a read-free key, no home for Accept-Encoding, slower hot path, authed-leak
surface), but the user's rebuttals held for the parts that mattered: no API
changes on *common* combinators (a clunkier `and_then` is acceptable), boxed
keyfn handled at the tower layer, and "a valid cache entry on a revoked token
is a platform bug" (the platform owns all side effects, so it's fixable, not
fatal). The "keyNext vs keyFn" distinction I introduced was dissolved by the
user — they're the same authoring act.

**D2 — Authored keys are the incumbent's status quo; derived keys need the
compiler.** The un-forgeable *derived* key ("what does this closure touch
without executing it") requires a Roc compiler touched-set API the user has
pushed for but which lacks buy-in. **Assumed unavailable for this design**
(explicit constraint). Consequence: keys stay request-derived (method +
path/query + negotiated encoding), which is correct for essentially every REST
handler; the derivation effort moves entirely to the **dep/invalidation side**,
where no compiler help is needed — the DB is fully owned and analyzable.

**D3 — Collection (table/column) granularity; no row tokens, no prefix
sweep.** Row tokens (`article:slug:foo`) are inert-or-unsound against the
exact-key `HashMap` purge — sweeping them needs an ordered/trie index ("prefix
sweep") that doesn't exist and isn't worth building. Over-purge at collection
granularity is **accepted**; it costs extra misses, never wrong data
(MAY-analysis principle: over-approximating reads or writes is always sound).

**D4 — Blessed-path scoping.** Cacheability derivation applies to the blessed
surface (coeffect reads via `Sql.query!`/generated readers, writes via
`DbWrite` effects). Raw nondeterministic host calls (`now_ms!`,
`random_bytes!`, `next_id!`, `http_send!`) poison the request's cacheability —
this is correct behavior, not a hole: "the fallback is inherently uncacheable
for the same reason this isn't done in any other framework."

**D5 — Credential safety is structural, not hash-strength.** The round-2
"implicit cacheability routes a credential into the cache" finding
(`GET /api/user` returns `{email, token}`) is resolved by the shipped
`bypass_headers: ["authorization"]` rule: any request bearing credentials
bypasses the cache entirely, so a credential-bearing response can never enter
a shared entry. Authed caching (per-token fragments) stays out of scope —
low cross-user hit rate, real leak surface, no need.

**D6 — Write-path completeness as a precondition (SHIPPED, `987de9d`).**
Invalidation completeness assumes every mutation flows through the `effect.rs`
choke point. Two escape hatches existed and are now gone: `Req.reserved_id!`
no longer writes via app-visible SQL (it delegates to `Host.next_id!`, a
host-owned persistent per-database sequence), and `Sql.exec!` is replaced by
`Sql.migrate!` (startup DDL only, by name and intent). Runtime writes:
`DbWrite` effects only. Multi-write / read-after-write handler support is
**explicitly deferred** — wanted (to avoid re-frame saga awkwardness), but
"designing something that will properly maintain the cache invariants can be
done later."

**D7 — The SQLite authorizer replaces every proposed parser (round-3 gem,
then generalized).** `rusqlite::Connection::authorizer` fires per-column
callbacks at *prepare* time: `Read{table, column}` for every column a
statement reads, `Insert/Update/Delete{table[, column]}` for every write —
**including writes performed by triggers** (SQLite compiles trigger programs
into the statement; `accessor` names the firing trigger). This is
authoritative and kills, in one move:
- the ~250-line trigger-body string parser the cascade-closure spec assumed;
- the `## @cache-cascade` annotation DSL and its fail-closed build errors;
- the *entire per-read backward cascade closure*: instead of expanding each
  read's deps backward across trigger edges, the **write side** derives its
  trigger-expanded write set directly — preparing `INSERT INTO favorites …`
  reports `Insert{favorites}` *and* `Update{articles.favorites_count,
  accessor: trg_fav_insert}`. Reads keep their honest column-level read sets;
  purge is the intersection. Column-scoping falls out for free (a
  `favorites` write purges only entries that read `articles.favorites_count`),
  answering the user's "if we have the exact table list, don't we know from
  the query analysis which columns are applicable?" — yes, on both sides;
- `effect.rs`'s `write_target_table` string parsing (demoted to fallback);
- the dynamic-path gap: `Sql.query!` SQL is derivable at prepare time the
  same way — the list/feed no longer needs the one hand-declared constant.

Known limits, all sound: `Insert`/`Delete` report no column list → treated as
writing every column of the table (collection-level purge); `WHEN`-guarded
triggers are compiled unconditionally → over-approximate; a `Read` with no
usable column → whole-table dep.

**D8 — Analysis shadow: derive against a rusqlite `:memory:` schema mirror,
never the live engine.** Turso has no authorizer, and taxing hot-path prepares
is unnecessary. The host keeps one in-memory rusqlite connection holding the
app schema (mirrored from init-time statements), with the authorizer
installed; dep/write-set derivation prepares against *it*, memoized per SQL
text. One derivation per unique statement, both engines served identically,
zero cost on the request hot path after warmup.

**D9 — Fail closed.** A statement the shadow can't prepare or attribute:
read side → the request is uncacheable; write side → conservative purge
(legacy table parse, else full cache flush) + log. Never silent
under-invalidation. (Round-3's fail-closed-attribution finding, generalized.)
*Round-4 correction:* "never silent staleness" was overclaimed — the
read-then-store vs write-then-purge race (a miss computed pre-write, inserted
post-purge) is inherited byte-identically from the shipped cache and is
silent, TTL-bounded staleness. Closed by the plan's R13: per-table generation
counters checked at insert (also closes torn multi-statement reads). Two
further silent-staleness holes found and closed: read-path DML must *purge*
not just poison (R2), and runtime DDL must flush the shadow + memos (R4).

**D10 — Automatic storage; delete the opt-in surface.** Deps ride the request
ctx and reach the middleware as a response extension; `Res.cacheable`, the
`x-tp-cache-tables` header, the `Db.*_read_tables` lists, and sqlgen's
`_tables` emission are all deleted. Opt-out is standard HTTP
(`Cache-Control: no-store`), not a custom API. The `ResponseCache` setting
(TTL backstop, max entries, methods, bypass_headers) remains the master
switch.

**D11 — Accepted regression: `follows` re-enters derived deps.** The shipped
hand-pruning dropped `follows` on a semantic argument (anon `following` is
constant-false) that a MAY-analysis of SQL can't reach. Derivation honestly
re-adds it wherever the SQL reads `follows`. Accepted per the platform lens.
*Round-4 correction — the honest scope is ~3× the original claim:* not just
by-slug but **profile** and **comments** too (both carry `following` EXISTS),
and comments additionally gain an `articles` dep (the `article_meta` pre-read)
— so a follow flushes all cached profiles + comment lists, and an article
INSERT flushes comment lists. The good news the original claim missed:
**list and tags derive *identically* to the hand-tuned lists** (the anon list
variant genuinely reads no `favorites`/`follows` — the materialized columns
carry them via trigger-expanded write sets instead). Idiomatic recovery is
anon variants of *three* queries (by-slug, profile, comments), out of scope
here. `bench/cache-check.sh` must flip the follow assertion AND assert the
profile/comments purge behavior explicitly.

**D12 — Instrumentation neutrality (user constraint, post-round-4).** Logs
and instrumentation/tracing must never invalidate the cache. This narrows
round-4's R3 from "any `DbWrite`/`HttpSend` in the effects list poisons" to
"any **Must** `DbWrite`/`HttpSend` poisons", and the platform's existing
effect tiers carry the whole distinction:
- `Log` effects and `Host.log!` are always neutral — no poison, no purge, at
  either tier.
- `Must` non-Log effects poison: "this effect must run" is incompatible with
  a cache hit that skips the handler, so Must-effectful responses stay
  uncacheable and their effects always fire.
- `Opt` effects never poison: they are best-effort by contract, so a hit
  skipping them is consistent with semantics the app already accepted.
  Soundness needs no poison because the write side covers them — a dedicated
  metrics/audit table is **cache-inert by derivation** (no cached entry reads
  it, so its purge is a no-op), while an Opt write into a cached table (a
  view counter on `articles`) self-invalidates through its derived write-set
  purge + the R13 generation check: the entry never survives insert, the
  route is effectively uncacheable, and its effects always fire. Both cases
  fall out of the machinery; no instrumentation allowlist exists.
- Documented trade: handler-level `Opt`/`Log` instrumentation does not
  observe cache hits. Per-request observability that must see hits belongs to
  the host layer (middleware tracing/access logging fires on hits and misses
  alike) — structurally the only place that can see a request the handler
  never ran for.

**D13 — Cookies become a coeffect; precise variant keying replaces the
cookie bypass (in scope).** The missing cookie coeffect was an oversight, and
its absence is what forced R1's blunt cookie-presence bypass. In scope for
this build:
- `Cookie` is **stripped from the raw header record** the handler receives —
  this is what makes the guarantee structural rather than advisory (a handler
  can no longer parse it by hand invisibly). Small breaking change to the
  request shape; accepted.
- Cookie access is host-mediated (`Req.cookie("name")`-shaped coeffect); each
  read self-reports the cookie *name* to the ctx alongside the SQL deps.
- The cache stores/matches **variants**, the same machinery R1's `Vary`
  support already requires: an entry records which cookie names (and varied
  headers) it depends on; lookup finds the base `(method, target)` entry,
  reads its variant spec, and matches on the request's values of exactly
  those cookies. A route that reads a `theme` cookie fragments only on
  `theme`; a route reading no cookies caches fully even for cookie-laden
  browser traffic — a real hit-rate win over the bypass (most anonymous
  browser requests carry analytics/consent cookies). A session-cookie read
  keys per-session: correct, low-value, same story as `authorization`.
- The hard-default `cookie` bypass is superseded by this; the
  `authorization` bypass stays (raw headers remain visible for everything
  except `Cookie`).

**D14 — Request body is a coeffect; it contributes to the key only when
extracted (in scope).** Supersedes R1's "never cache non-empty-body requests"
clause entirely. The body is pure request state, so *keying* on it is as
sound as keying on the query string — but blanket hashing (or blanket
exclusion) punishes routes that never look at the body for bytes a client
happened to send. Instead, the body joins cookies as a self-reporting request
coeffect on the D13 variant machinery:
- `body` is **stripped from the raw handler record** (currently a plain
  `body : List(U8)` field — invisible pure access); access is host-mediated
  (`Req.body!`/`json_body!` resolve through the ctx), which is what makes
  "didn't extract it" platform-known rather than convention.
- An extracting read self-reports to the ctx; the entry's variant spec gains
  a body dimension; lookup matches on the hash of the raw body bytes. No
  canonicalization — `{"a":1,"b":2}` vs `{"b":2,"a":1}` fragment into two
  entries, a miss cost with exact parity to the incumbent's raw query-string
  keying, never staleness.
- A route that never reads the body caches normally regardless of what the
  client sent; a body-reading route fragments per distinct body. Combined
  with the existing guards (a writing POST purges via its derived write set;
  a Must-effectful POST poisons via D12), adding `"POST"` to the methods
  config yields sound, automatically-invalidated **GraphQL/search read
  caching** — the strongest single demo of caching-for-every-app, and
  something blanket exclusion foreclosed.
- `MaxBodyBytes` remains the size guard; an unread body is never hashed.

## Review-arc ledger

| Round | Proposal | Verdict then | Standing now |
|---|---|---|---|
| 1 | Coeffect memoization (row tokens, key threading, authed keys) | Rejected (4/4) | Model kept (D1); row tokens dropped (D3); authed out (D5); keys deferred to compiler API (D2) |
| 2 | Derived deps, no compiler API (runtime parsers, token-in-key) | Rejected (3 reviewers) | Baseline objection overturned; parsers superseded by authorizer (D7); credential fix structural (D5); survivor lint subsumed by full derivation |
| 3 | Column-scoped backward cascade closure | "Buildable, not worth it yet" | "Inert on demo" = wrong baseline (overturned); authorizer finding promoted from gem to foundation (D7, D8); closure itself made unnecessary by write-side expansion |
| 4 | This design (authorizer + shadow, automatic) | **Build with revisions** (4 reviewers) | Foundation source-verified (prepare fires trigger sub-programs, rusqlite 0.32.1 + SQLite 3.46.0); 16 mandated revisions R1–R16 in the plan banner — 3 soundness holes (shared-cache HTTP posture, read-path writes must purge, effectful GETs must poison), 1 dead-wiring fix (deps via oneshot), D9/D11 corrected below |

## Deferred (explicitly, do not fold into the build)

- Multi-write + read-after-write in `Req => Res` handlers (D6).
- Compiler touched-set API — would close the authored-key forgeability for
  non-standard variance; not needed for dep derivation.
- Row-token precision / prefix sweep (D3).
- Authed response caching (D5).
