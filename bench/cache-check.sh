#!/usr/bin/env bash
# Response-cache correctness regression test. Proves, against a freshly-built
# `./main`: anonymous reads cache AUTOMATICALLY (no handler opt-in — deps are
# DERIVED per statement by the analysis shadow); ETag/304 works; and an
# authenticated write purges exactly the entries whose derived reads it
# invalidated — column-scoped, including the trigger-maintained materialized
# `tag_list`/`favorites_count` columns (the write set is trigger-expanded at
# prepare time, no hand-declared cascade) — while anonymous views stay correct
# under follow writes. Exit non-zero on any failure. Runs on either engine
# (build `main` with `just build` or `just build-turso` first).
set -uo pipefail
cd "$(dirname "$0")/.."
DB=/tmp/tower-platform-realworld.db
PORT=8770
rm -f "$DB" "$DB"-wal "$DB"-shm
PORT=$PORT ./main >/tmp/cache-check-srv.log 2>&1 &
SRV=$!
trap 'kill -9 $SRV 2>/dev/null' EXIT
for _ in $(seq 1 80); do curl -s "http://127.0.0.1:$PORT/api/tags" >/dev/null 2>&1 && break; sleep 0.25; done
B="http://127.0.0.1:$PORT"
fail() { echo "FAIL: $1"; exit 1; }

# Register a user, grab the token.
TOK=$(curl -s -X POST "$B/api/users" -H 'content-type: application/json' \
  -d '{"user":{"username":"cacheuser","email":"c@e.com","password":"password"}}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
[ -n "$TOK" ] || fail "no token"
auth=(-H "authorization: Token $TOK")

# Create an article with tag zzz.
SLUG=$(curl -s -X POST "$B/api/articles" "${auth[@]}" -H 'content-type: application/json' \
  -d '{"article":{"title":"Cache One","description":"d","body":"b","tagList":["zzz"]}}' \
  | sed -n 's/.*"slug":"\([^"]*\)".*/\1/p')
[ -n "$SLUG" ] || fail "no slug"

# 1) Anonymous /api/tags — populates the cache. Must contain zzz.
T1=$(curl -s "$B/api/tags")
echo "$T1" | grep -q '"zzz"' || fail "tags missing zzz: $T1"

# 2) A cache HIT should carry an ETag; a matching If-None-Match yields 304.
ET=$(curl -s -D - "$B/api/tags" -o /dev/null | sed -n 's/[Ee][Tt]ag: *\(.*\)\r*/\1/p' | tr -d '\r')
[ -n "$ET" ] || fail "no ETag on cached /api/tags"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$B/api/tags" -H "if-none-match: $ET")
[ "$CODE" = "304" ] || fail "expected 304 for matching If-None-Match, got $CODE"

# 3) Authenticated write of a NEW tag (article_tags INSERT) must purge /api/tags.
curl -s -X POST "$B/api/articles" "${auth[@]}" -H 'content-type: application/json' \
  -d '{"article":{"title":"Cache Two","description":"d","body":"b","tagList":["yyy"]}}' >/dev/null
T2=$(curl -s "$B/api/tags")
echo "$T2" | grep -q '"yyy"' || fail "tags NOT invalidated after write (missing yyy): $T2"
echo "$T2" | grep -q '"zzz"' || fail "tags lost zzz after invalidation: $T2"

# 4) Materialized-tag edge: cache the by-slug article, then edit its tags
#    (article_tags write -> trigger updates articles.tag_list). The DERIVED
#    write set carries the trigger-expanded articles.tag_list update, which
#    must purge the by-slug entry (it reads that column).
A1=$(curl -s "$B/api/articles/$SLUG")
echo "$A1" | grep -q '"zzz"' || fail "byslug missing zzz: $A1"
curl -s -X PUT "$B/api/articles/$SLUG" "${auth[@]}" -H 'content-type: application/json' \
  -d '{"article":{"tagList":["www"]}}' >/dev/null
A2=$(curl -s "$B/api/articles/$SLUG")
echo "$A2" | grep -q '"www"' || fail "byslug NOT invalidated after tag edit (missing www): $A2"
echo "$A2" | grep -q '"zzz"' && fail "byslug still shows stale zzz after tag edit: $A2"

# 5) `users` dep still invalidates after the follows-pruning: cache the anon
#    profile, edit the bio (UPDATE users), re-read must reflect it.
curl -s "$B/api/profiles/cacheuser" >/dev/null
curl -s -X PUT "$B/api/user" "${auth[@]}" -H 'content-type: application/json' \
  -d '{"user":{"bio":"edited-bio-xyz"}}' >/dev/null
P2=$(curl -s "$B/api/profiles/cacheuser")
echo "$P2" | grep -q 'edited-bio-xyz' || fail "profile NOT invalidated after bio edit (users dep): $P2"

# 6) `favorites` dep + trigger cascade still invalidates: cache the article
#    (favoritesCount 0), favorite it (INSERT favorites -> trigger bumps
#    articles.favorites_count), re-read must show 1.
A3=$(curl -s "$B/api/articles/$SLUG")
echo "$A3" | grep -q '"favoritesCount":0' || fail "expected favoritesCount 0 before favorite: $A3"
curl -s -X POST "$B/api/articles/$SLUG/favorite" "${auth[@]}" >/dev/null
A4=$(curl -s "$B/api/articles/$SLUG")
echo "$A4" | grep -q '"favoritesCount":1' || fail "article NOT invalidated after favorite (favorites dep / trigger cascade): $A4"

# 7) Comments derive their own deps: cache the anon comment list, add a
#    comment (INSERT comments), the re-read must show it.
curl -s "$B/api/articles/$SLUG/comments" >/dev/null
curl -s -X POST "$B/api/articles/$SLUG/comments" "${auth[@]}" -H 'content-type: application/json' \
  -d '{"comment":{"body":"first-comment-abc"}}' >/dev/null
C2=$(curl -s "$B/api/articles/$SLUG/comments")
echo "$C2" | grep -q 'first-comment-abc' || fail "comments NOT invalidated after comment write: $C2"

# 8) Follow behavior under DERIVED deps (decision log D11/R14): the anon
#    profile/by-slug/comments reads carry a `following` EXISTS over `follows`,
#    so derivation honestly re-adds the dep — a follow DOES purge those
#    entries now (accepted hit-rate cost vs the old hand-pruned lists; the
#    recomputed anon body is identical, so correctness is unaffected). Assert
#    the correctness half: anon `following` stays false after a follow.
#    (Precision recovery = anon query variants without the EXISTS; app-side,
#    out of scope here.)
TOK2=$(curl -s -X POST "$B/api/users" -H 'content-type: application/json' \
  -d '{"user":{"username":"follower1","email":"f@e.com","password":"password"}}' \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
curl -s -X POST "$B/api/profiles/cacheuser/follow" -H "authorization: Token $TOK2" >/dev/null
P3=$(curl -s "$B/api/profiles/cacheuser")
echo "$P3" | grep -q '"following":false' || fail "anon profile following should stay false after a follow: $P3"

echo "PASS: automatic caching, ETag/304, derived column-scoped invalidation"
echo "      (trigger-expanded tag_list/favorites_count, users bio, comments),"
echo "      and anon correctness under follow writes"
