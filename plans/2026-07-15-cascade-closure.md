# Spec: column-scoped backward trigger-cascade closure (build-time)

> **Round-3 adversarial review, 2026-07-15. Buildable but not worth it *yet* on
> this workload; two findings are reusable regardless.**
>
> **Reusable gem — use the SQLite AUTHORIZER, not a string parser.** rusqlite
> 0.32.1 has `Connection::authorizer`; preparing a firing DML (`INSERT INTO
> favorites DEFAULT VALUES`) with the callback installed fires once per column a
> trigger writes — `Update { table_name, column_name, accessor: Some(trigger) }` —
> authoritative, column-level, skips nested subqueries. This replaces the fragile
> ~250-line trigger-body parser §1 assumed. Hard limit: the authorizer reports
> `Insert { table }` with NO column list → INSERT-target triggers are table-only,
> not column-scoped.
>
> **Soundness fix — attribution must fail-CLOSED.** After materialization
> `article_tags` is not a base table of the by-slug read (it's a plain `articles`
> column), so it enters ONLY via cascade attribution — no base-table backstop. An
> expression-wrapped materialized column (`COALESCE(a.tag_list,'')`) fails to
> attribute → silently drops the dep → stale. A materialized column reached
> through anything but a bare `Column` opcode must be a build error, not a silent
> pass. Also add a build check that each trigger body reads no table outside its
> `ON`-table (else the "source = ON-table" restriction is unsound).
>
> **Why it's not worth building now (rounds 2 & 3 converged):** deletes 3 list
> literals; column-scoping is INERT (every article read selects both materialized
> columns); adds no correctness `bench/cache-check.sh` doesn't already prove; and
> the dynamic `Sql.query!` list/feed — the purest materialized read, no base-table
> backstop — is scope-excluded, so the guarantee misses the read that most needs
> it. Under a platform lens the residuals that survive are: the dynamic path isn't
> sqlgen-analyzed, and INSERT-target column-scoping is unbacked.
>
> **Convergent recommendation: a build-time subset LINT instead** — assert
> `hand-declared deps ⊇ analyzer-resolved base tables − annotated prunes` (a prune
> = one line, e.g. `## @cache-prune follows: anon-constant-false`). Catches the
> real drift hazard (edit a query's JOIN/EXISTS, forget the dep) using the
> `_tables` sqlgen already emits; preserves the follows-pruning; no trigger
> extractor, no attribution pass, no DSL, no fail-closed codegen. The stable
> trigger-cascade deps (`article_tags`, `favorites`) stay hand-added (schema
> triggers drift far less than queries). Spec below retained for context.

---


Derive each generated read's cache dependency set — including the tables that
feed its **materialized / trigger-maintained columns** — at build time in
sqlgen, column-scoped so it doesn't over-broaden, replacing the hand-declared
`Db.*_read_tables`. Over-approximate where it must be (accepted: automatic
correctness for every app beats hand-tuned precision on the example), but *fairly*
restricted. Uses only sqlgen + the schema it already owns; no compiler API, no
runtime SQL parser, no cache re-key.

## Scope / non-goals

- **In:** generated `## @sql` reads. Their `_tables` becomes cascade-expanded and
  column-scoped; the hand-declared `Db.*_read_tables` for generated reads are
  deleted. The cache, the `Res.cacheable`/header path, and the `effect.rs`
  write-side (emit the written table token) are **unchanged** — the cascade lives
  entirely on the *read* side, so writes need no forward-closure logic.
- **Out:** row-level tokens + prefix sweep (collection granularity only, exact-key
  match — see the prefix-sweep note in the entity-tokens discussion); key
  derivation (default key + authored, unchanged); authed/blessed cacheability
  (separate); a runtime FROM/JOIN parser for the dynamic `Sql.query!` list/feed
  path — that path keeps a single hand-declared dep constant (the one manual spot;
  a host parser is possible later but its fragility isn't worth it now).

## Mechanism

### 1. Trigger edge extraction (build-time, sqlgen)
sqlgen already applies `## @schema` (triggers included) to a real SQLite. For each
`CREATE TRIGGER`, extract forward edges `(source_table -> target_table.column)`:
- **source** = the trigger's `ON <table>` — the table whose write *fires* it. This
  is deliberately the firing table, NOT the tables the trigger *body reads*: a
  materialized value is a function of what makes the trigger fire, so chasing body
  reads is exactly the over-broadening to avoid.
- **targets** = each `UPDATE <t> SET <col> = …` / `INSERT INTO <t>(<cols>)` in the
  body → one edge per written column.
- Multi-statement bodies → multiple edges. A `WHEN` guard or conditional body →
  the edge is taken unconditionally (can't know at build time if it fired;
  over-approximate, safe).
- **Fail-closed:** a trigger whose `(source, target.column)` sqlgen can't cleanly
  extract (dynamic target, exotic shape) is a **build error** unless the app
  supplies an explicit annotation:
  ```
  ## @cache-cascade favorites -> articles.favorites_count
  ```
  No silent under-closure. `ON DELETE/UPDATE CASCADE` foreign keys are treated as
  cascades only if declared this way — never auto-inferred (that's the FK-smear
  trap).

RealWorld edges this yields: `favorites -> articles.favorites_count`,
`article_tags -> articles.tag_list` (from `trg_fav_insert/delete`,
`trg_tag_insert/delete`).

### 2. Result-column attribution (existing analyzer, small extension)
For each read, sqlgen walks each result register to its source `Column(cursor,
cid)` opcode → `cursor`→table (`resolve_rootpage`) → `cid`→column name
(`pragma_table_info`, already queried for typing). A result column that is a plain
table column attributes to `(table, column)`; an expression / subquery result
does not attribute (and therefore gates no cascade — correct).

### 3. Per-read dependency derivation
```
deps(read) = { base tables the plan scans }                     # already emitted as _tables
           ∪ { X : read selects column T.c AND edge X -> T.c }  # column-scoped cascade
```
Emit as the read's `_tables` (collection tokens = table names). This is the whole
change: `_tables` goes from "tables scanned" to "tables scanned + cascade sources
of the columns actually selected."

**Column-scoping is the restriction you asked for:** a read of `articles.tag_list`
gets an `article_tags` dep; a read of `articles.title` that never selects
`tag_list` does **not** — a tag write won't purge it. A join table cascades only
through the specific column its trigger maintains, to only the reads that select
that column. It does not smear onto every table touching the join table. The one
unavoidable over-broadening is `SELECT *`/`a.*` (inherits every cascade source of
the table — the honest cost of a wildcard select).

### 4. Consumption (unchanged plumbing)
Handlers keep calling `Res.cacheable(DbGen.<q>_tables)`; the emitted constant is
now cascade-expanded. `Db.article_read_tables` / `profile_read_tables` /
`comment_read_tables` (hand-declared) are deleted in favor of the generated
constants. The dynamic list/feed keeps its one hand-declared `list_read_tables`.

## Worked result (RealWorld)

| read | derived deps | note |
|---|---|---|
| by-slug (`article_select`) | `articles, users, favorites, follows, article_tags` | `follows`/`favorites` via the `following`/`favorited` `EXISTS` subqueries (honest — the SQL reads them); `article_tags` via cascade (selects `tag_list`) |
| anon list (`article_summary_select_anon`) | `articles, users, favorites, article_tags` | **no `follows`** — the anon variant has no `following` subquery, so it genuinely never reads `follows`; `favorites`/`article_tags` via cascade on the materialized columns |
| profile | `users, follows` | `following` subquery |
| comments | `comments, users, follows` | `following` subquery |
| tags | `article_tags` | direct |

The accepted `follows` over-purge is thus **localized** to the by-slug / profile /
comments reads that carry a `following` `EXISTS`; the list/feed and tags are
unaffected by derivation. That's the honest cost, and it's smaller than "every
read re-adds follows."

## What it buys
- **Cascade-correct by derivation** — deletes the hand-added `article_tags`
  supplement; a materialized column can't silently go stale because a human forgot
  its source table.
- **Drift-safe** — edit a query to join/select a new table and its deps track
  automatically; the hand list can't drift out of sync with the SQL.
- **Column-precise** — cascades reach only reads that select the maintained
  column, not the whole table.
- **Automatic for generated reads**, over-approximate only where SQL semantics
  force it (constant-false `EXISTS` subqueries, `SELECT *`, `WHEN`-guarded
  triggers).

## Open / to pressure-test
- Trigger-body extraction robustness (the round-2 reviewer noted `write_target_table`
  returns `None` on `CREATE …`; this needs a real body walk, fail-closed on miss).
- Column attribution for the materialized columns specifically (they are plain
  `articles` columns post-materialization, so they should attribute cleanly — verify).
- The dynamic list/feed staying hand-declared (is one manual constant acceptable,
  or does it undercut "automatic"?).
