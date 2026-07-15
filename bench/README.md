# End-to-end benchmark harness

Reproducible wrk benchmark for the RealWorld example, used to compare host
variants (e.g. the pre-arena `main` baseline against the `request-arena`
branch) on a byte-identical seeded database.

## Files

- `seed.py` — deterministic seed (`random.seed(1234)`): 200 users, 5000
  articles, 10000 comments, plus tags/favorites/follows. The schema is
  identical across host variants, so one seeded file drives every binary.
- `run.sh` — wrk driver. Boots each server on its own port against a fresh
  copy of the seeded DB, warms it, then measures four read endpoints and prints
  `label endpoint rps p50` rows.

Generated artifacts (`seeded.db`, `srv-*` binaries) are gitignored — rebuild
them locally.

## Running

```sh
# 1. Seed once (deterministic).
python3 bench/seed.py bench/seeded.db

# 2. Build the current branch's server.
just build examples/realworld/main.roc && cp main bench/srv-arena

# 3. Build a baseline from another ref via a worktree (keeps this tree intact).
git worktree add /tmp/tp-main main
(cd /tmp/tp-main && just build examples/realworld/main.roc)
cp /tmp/tp-main/main bench/srv-main
git worktree remove /tmp/tp-main

# 4. Compare.
bench/run.sh main:bench/srv-main arena:bench/srv-arena
```

## Endpoints

| label      | path                                          | shape                         |
|------------|-----------------------------------------------|-------------------------------|
| `byslug`   | `/api/articles/article-2785`                  | single-row typed object       |
| `profile`  | `/api/profiles/user14`                        | single-row typed profile      |
| `list`     | `/api/articles?limit=20`                      | 20-row typed list (dynamic)   |
| `comments` | `/api/articles/article-2785/comments`         | multi-row typed list          |
| `tags`     | `/api/tags`                                   | flat typed string array       |

All endpoints read typed rows and serialize the envelope in Roc (the `@sql json`
variant was removed — see `notes/2026-07-15-typed-vs-json.md`). `list` and
`comments` are the multi-row shapes whose response bodies the request arena
keeps zero-copy; `byslug`/`profile` are the small-body shapes that the
slow-drain heuristic copies (see `crates/host/src/body.rs`).

## Load parameters

`run.sh` uses `wrk -t4 -c16 -d15s` after a 3s warm pass — a single laptop core
count's worth of concurrency, enough to expose per-request host CPU (the
malloc/free bucket the arena targets) without saturating on network. Adjust the
threads/connections in `run.sh` for other hardware.
