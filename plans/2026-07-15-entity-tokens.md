# Plan: tighten the shipped response cache (prune spurious deps; entity tokens)

> The salvageable kernel from the rejected coeffect-memoization design
> (`plans/2026-07-15-coeffect-memoization.md`). Keeps the shipped tower-layer
> cache — with its compression/Accept-Encoding keying, ETag/304, and
> `authorization` bypass — and improves its invalidation *precision* without any
> of the rejected design's machinery (no key pass, no second FFI entrypoint, no
> combinator rewrite, no cross-user-leak surface).
>
> The reverse index is already `token(String) -> entries` (`cache.rs`), and the
> token is opaque — it does not have to be a table name. That's the whole lever.

## Honest framing

The adversarial review established two things about *this* workload:
- **Collections must stay table-scoped.** Row/entity tokens can't invalidate a
  set-query on INSERT (a new row emits a token no cached list recorded) or a
  trigger-cascaded materialized column. So list/feed/tags keep table tokens —
  that's correct, not a compromise.
- **The entity-granularity win is marginal here.** byslug is already ~1.9×
  (near the ~120k hit-path ceiling); reads vastly outnumber writes; a TTL
  backstop exists. Entity scoping moves hit-*rate* under writes, not hit-*cost*,
  and only pays on a write-heavy single-entity-read profile the platform hasn't
  demonstrated.

So Phase 1 (prune spurious deps) is a clean, do-it-now correctness/efficiency
win. Phase 2 (entity tokens) is specced but **gated** — build only if a
write-heavy single-entity workload justifies the write-side surface.

## Phase 1 — prune the spurious `follows` dependency (DONE 2026-07-15)

> Shipped: `Db.roc` `*_read_tables` hand-declared without `follows`; committed a
> durable regression test `bench/cache-check.sh` covering ETag/304, tag_list /
> users-bio / favorites+trigger-cascade invalidation, and follows-pruning
> soundness. 154/154 both engines; cache-check PASS both engines.

**Why it's spurious.** The cache only stores **anonymous** responses (authed
requests carry `authorization` and bypass it). For an anonymous viewer the
`following`/`favorited` flags are constant-false — they read `follows`/the
viewer's `favorites` but the *result* is always false regardless of those
tables' contents. So `follows` can never change a cached body, yet it's in every
cached dep set (`Db.roc` `*_read_tables`), so every follow/unfollow needlessly
purges most of the cache.

**Change** — `examples/realworld/Db.roc`, the `*_read_tables` constants only:
- `list_read_tables`: `["articles", "users", "favorites", "follows", "article_tags"]` → drop `follows` → `["articles", "users", "favorites", "article_tags"]`. (`favorites`/`article_tags` stay — they feed the materialized `favorites_count`/`tag_list` the anon list *does* show.)
- `article_read_tables`: hand-declare `["articles", "users", "favorites", "article_tags"]` (was `DbGen.article_by_slug_q_tables.concat(["article_tags"])` = `[articles, favorites, follows, users, article_tags]`) → drop `follows`.
- `profile_read_tables`: `DbGen.profile_by_username_q_tables` = `["follows", "users"]` → `["users"]`.
- `comment_read_tables`: `DbGen.comments_q_tables` = `["comments", "follows", "users"]` → `["comments", "users"]`.
- `tags_read_tables`: unchanged (`["article_tags"]`).

Document the coupling: this is valid **because cached responses are anonymous**
(the `authorization` bypass). If authed caching is ever added, `follows` becomes
a real dep again — revisit.

**Verify:**
- 154/154 conformance both engines (unchanged behavior for correctness).
- Extend `scratchpad/cache-check.sh`: a follow/unfollow write must **not** purge
  a cached article/list/profile/comments entry (assert the entry still hits /
  ETag unchanged across a follow), while a bio edit (`users`) and a favorite
  (`favorites`) still do.
- Optional: a quick wrk A/B under a trickle of follow writes showing higher
  sustained hit rate vs today (the concrete payoff).

## Phase 2 — entity tokens for single-entity reads (GATED, spec only)

**Goal.** Scope by-slug / profile invalidation to the one entity instead of the
whole table, so an edit/favorite on article 5 purges only `article:5`'s entry,
not every cached article. Collections stay table-scoped (see framing).

**Read side** (`sqlgen` + `Db.roc`): for a single-row read whose identity is a
bound param (`article_by_slug_q` binds `:slug`, `profile_by_username_q` binds
`:username`), sqlgen emits an entity token beside the table tokens —
`article:slug:<slug>` (and, post-resolution, `article:id:<id>` once the row is
read). `Res.cacheable` keeps taking the token list; the read contributes both
its table tokens (for cascade-safety) and its entity token.

**Write side** (the new surface, and why it's gated): a write must emit the same
entity token to match. Today `effect.rs` parses only the *table* from write SQL.
Two options:
- (a) **Declare on the effect**: `DbWrite` (or a `Res.fx` variant) carries an
  optional invalidation-token list the handler fills from the entity it already
  loaded (`article:slug:foo`, `article:id:5` — over-emit both). Explicit, but
  adds a field to the effect ABI and asks every mutating handler to declare.
- (b) **Parse id from write params**: effect.rs extracts the entity id from the
  write's WHERE/VALUES params. Fragile (positional params, joins), and can't
  produce the *slug* token a by-slug read used. Not recommended.

Recommend (a) if built. Cascade note: a favorite/tag write must over-emit the
*article's* entity token (it holds the loaded article), or the materialized
`favoritesCount`/`tagList` on a by-slug entry goes stale — the same
trigger-cascade trap that sank the row-level design, avoided here only by
over-emitting the article token on writes to its child tables.

**Gate:** do not build Phase 2 unless a workload shows by-slug/profile hit rate
materially dented by table-scoped purges. On the RealWorld read-heavy profile it
is not justified.

## Impact
- Modify (Phase 1): `examples/realworld/Db.roc`, `scratchpad/cache-check.sh`.
- Modify (Phase 2, if built): `crates/sqlgen/src/emit.rs` (+analyze), `platform/Res.roc`
  or `platform/main.roc` (effect invalidation-token field), `crates/host/src/effect.rs`,
  `examples/realworld/*` mutating handlers.
- Risk (Phase 1): none beyond the anonymous-only coupling (documented + tested).
- Risk (Phase 2): write-side ABI change; cascade over-emit must be gotten right.

## Not doing
- Coeffect-layer memoization (rejected — see the other plan's banner).
- Caching authenticated/personalized responses (the `authorization` bypass
  stays; keying personalized bodies on a bearer hash is a leak surface for ~0
  shared-hit gain).
