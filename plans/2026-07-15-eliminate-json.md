# Plan: typed-read GET endpoints vs `@sql json` (the elimination bench)

## Summary
Give every RealWorld GET endpoint a typed-read + Roc-serialize path alongside the
existing `@sql json` path, A/B them end-to-end on both engines, and — if typed
wins or ties — delete the `@sql json` variant. Tests the thesis that the DB's
JSON aggregate engine is the movable cost and Roc's serializer can replace it.

## Impact
- Modify: `examples/realworld/{Api,Db,DbGen,Articles,Profiles,Comments,main}.roc`
- Create: `notes/2026-07-15-typed-vs-json.md`
- Risks: sqlgen `@sql` (many) untested in-tree (fallback: `Sql.query!`); typed
  list must match the json path's body-less summary shape for conformance;
  toggle must keep BOTH paths compiling.

## A/B mechanism
One constant `Api.read_mode = Typed | Json` (Api.roc, imported everywhere). Each
GET `_h!` binds `if Api.read_mode == Typed then <typed cofx> else <json cofx>`.
Flip + rebuild → two bench binaries; 154/154 verifiable on each mode.

## Phases
Phase 1: scaffold + the already-typed endpoints — files: Api, Articles, Profiles
  - `Api.read_mode` constant
  - `Articles.get` typed variant off existing `loaded_article!` → `Res.json(200, {article})`
  - `Profiles` `loaded_profile!` cofx + typed `get` → `Res.json(200, {profile})`

Phase 2: comments + tags typed — files: Db, DbGen (regen), Comments, main
  - `comments_q` `@sql` (many) + Roc `{comments:[...]}` assembly
  - `tags_q` `@sql` (many) + Roc `{tags:[...]}` assembly

Phase 3: list + feed typed — files: Db, Articles
  - dynamic `Sql.query!` over `article_select`, by-name row→`Db.Article`,
    Roc `{articles:[...],articlesCount:n}` assembly

Phase 4: bench rusqlite — files: notes/
  - build typed + json binaries (toggle flip), wrk A/B, write note

Phase 5: bench turso + decide — files: notes/, memory
  - typed + json turso binaries, A/B both engines, report; delete json if typed wins

## Testing Strategy
- Per phase: `just sqlgen-check` + roc-test + 154/154 conformance in BOTH modes
- Final: wrk A/B (byslug, profile, list, comments, tags) × {rusqlite, turso} × {typed, json}

## Assumptions
- [ASSUMPTION: extra `body` field in typed list summaries won't fail conformance —
  verify in Phase 3, trim if it does]
- [ASSUMPTION: sqlgen `@sql` many compiles cleanly — fall back to `Sql.query!` if not]
