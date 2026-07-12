# RealWorld example: the coeffect API under a real spec

**Goal**: build the RealWorld (Conduit) backend as a stress test of the
value-resolver coeffect API — pause on anything that doesn't fit cleanly.
**Outcome**: all 19 endpoints, every handler pure, 154/154 requests passing
the official Hurl conformance suite (`just realworld-conformance`).

## Decisions (grill session)

- **Auth**: opaque tokens (hex of `random_bytes!`) in a `tokens` table — a
  DB-backed memoized coeffect exercises the API better than JWT signature
  checks; the conformance suite treats tokens as opaque. Host gained
  `password_hash!`/`password_verify!` (argon2id; hosted keys WITHOUT the
  `roc_` prefix).
- **Scope**: full spec — only that unlocks the official suite as an oracle.
- **Placement**: generic pieces → platform (`Sql`, `NullableStr`, `JsonBool`,
  `Req.now!`, `Req.query_or`, `Cofx.map_reject`); domain pieces → app
  (auth coeffects, ISO-8601, slugify, error shapes).
- **Testing**: Rust e2e journey in the gate; official suite on the side.

## What fit cleanly (the API held up)

- **Auth as memoized coeffect** — `Auth.lookup!` memoizes under
  `"auth.viewer"`; `current_user!` (reject 401) and `optional_user!`
  (resolve `[Anon, Found(u)]`) are two views of one query.
- **Authorization as a resolver** — `owned_article!` = current_user! +
  load + author check: 401/404/403 expressed once, shared by PUT and DELETE.
- **Validation in coeffects** — a rejection IS the response, so field-keyed
  422s (`Api.nonblank("title", …)?`) compose with `?` naturally.
- **Multi-write transactions** — create-article-with-tags, register+token,
  delete-cascade, tag replacement: all just `Res.fx(...).fx(...)` chains;
  the host's single BEGIN IMMEDIATE group gives atomicity for free.
- **Partial updates** — `Try(Str, [Missing])` fields decode absent-vs-present;
  `Try(NullableStr, [Missing])` distinguishes absent / explicit null / value
  (the spec's bio/image tri-state) once NullableStr gained `parser_for`.
- **Read-before/write-after races** — favoritesCount is computed as
  count±1 from the pre-write read; last-write-wins on concurrent PUTs.
  Accepted (same trade re-frame makes).

## What didn't fit (found + fixed or filed)

1. **Derived JSON encoder stringifies Bool** (`"True"` instead of `true`)
   even though decode special-cases it. Filed as
   `2026-07-12-compiler-bool-encode-gap.md`; worked around with the
   platform `JsonBool` nominal (delete after the compiler fix).
2. **JSON null output** — derived tag-union encoding can't produce `null`;
   a custom `encoder_for` on a monomorphic nominal (`NullableStr`) can. The
   GENERIC `Nullable(a)` is impossible today: a user `where a.encoder_for`
   constraint isn't satisfiable by derivation (`Str` "has no method").
3. **Row decoding at scale** — hand-matching `List(SqlValue)` positions was
   untenable at 10+ columns; the `Sql` module (named-column access → `Try`)
   is the platform-shaped fix.
4. **`List.fold` is pure-only** — an effectful fold over schema statements
   doesn't type-check; sequential `?`-chained statements read better anyway.
5. **Numeral/value-restriction pins** — un-suffixed literals inside handler
   arithmetic (`count + 1`) and unpinned `Str` params in interpolated JSON
   leave method-constrained type vars that break encoder derivation; pin
   with `1.I64` / a local `x : Str` annotation.

## Conformance suite notes

The spec repo (now `realworld-apps/realworld`) retired the Postman
collection; the source of truth is a **Hurl** suite
(`specs/api/hurl/*.hurl`), and it asserts much more than the legacy one:
201/204 status codes, 409 duplicates keyed by column
(`errors.email = ["has already been taken"]`), `errors.token = ["is missing"]`
401s, blank-field 422s, a NIST 8-char password floor, empty-string
bio/image normalizing to null, tagList replacement semantics (absent =
keep, `[]` = clear, `null` = 422), and body-less article list items.
`just realworld-conformance` downloads the suite once into
`target/realworld-spec` and runs it against a fresh server on :8199.

## Perf pass (2026-07-12, commit 3b60503)

Seeded 5k articles / 200 users / 10k comments, wrk -c16. Low-hanging fruit
found and fixed:

1. **Missing indexes** (the big one): `favorites(article_id, user_id)` — the
   article SELECT pays a count subquery AND a viewer-EXISTS per row, both
   were full scans; `articles(author_id)`, `article_tags(tag)`,
   `comments(article_id)`. List 911→5.2k, comments 5.6k→41.6k req/s.
2. **Filter predicate shape is per-QUERY, not per-filter**: the page query
   wants EXISTS (planner keeps the ORDER BY id DESC scan and early-exits at
   LIMIT; IN materializes every match and computes the SELECT-clause
   subqueries for ALL of them first) — except sparse-by-nature filters
   (favorited-by-user) where the IN-list is tiny and wins. The count query
   wants IN. A tag-only count is a bare index-range count over
   article_tags — ~10x the join count's throughput under concurrency.
3. **reserved_id! fast path** (platform): UPDATE..RETURNING first, setup
   only on a fresh DB (3 statements → 1 per reservation).

Dead ends worth remembering: ANALYZE changed nothing; WAL checkpoint
changed nothing (contention was query cost, not wal-index size); a Python
threading harness bottlenecks at ~1k qps on its own GIL — only its
relative orderings are usable, wrk against the real server is the truth.
Residual (not low-hanging): argon2 makes register/login ~100ms by design;
create-article is fsync-bound; the 4 correlated SELECT subqueries per
article row could become window/join aggregates if lists ever need more.
