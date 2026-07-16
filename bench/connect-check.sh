#!/usr/bin/env bash
# Connect-protocol (unary + JSON) demo correctness test. Proves, against a
# freshly-built `./main` (build with `just build examples/connect/main.roc`):
# the unary wire shape (POST /Service/Method, JSON in, JSON out), the Connect
# error envelope + HTTP-status mapping, and — the point of the demo — that a
# read-only RPC is cached on its request-body hash (D14) while a mutation RPC's
# DbWrite invalidates exactly the cached reads it touched. Exit non-zero on any
# failure. Runs on either engine (build with `just build` or `just build-turso`).
set -uo pipefail
cd "$(dirname "$0")/.."
DB=/tmp/tower-platform-connect.db
PORT=8791
rm -f "$DB" "$DB"-wal "$DB"-shm
PORT=$PORT ./main >/tmp/connect-check-srv.log 2>&1 &
SRV=$!
trap 'kill -9 $SRV 2>/dev/null' EXIT
SVC="http://127.0.0.1:$PORT/tower.connect.v1.CatalogService"
for _ in $(seq 1 80); do curl -s -X POST "$SVC/ListBooks" -d '{}' >/dev/null 2>&1 && break; sleep 0.25; done
hdr=(-H 'content-type: application/json' -H 'connect-protocol-version: 1')
fail() { echo "FAIL: $1"; exit 1; }

# POST an RPC. Sets globals BODY / HTTP_CODE / ETAG (must NOT be called inside
# `$(...)`, which would run it in a subshell and drop the assignments).
BODY=""; HTTP_CODE=""; ETAG=""
rpc() { # method json
    local out
    out=$(curl -s -D /tmp/connect-h -w '\n%{http_code}' -X POST "$SVC/$1" "${hdr[@]}" -d "$2")
    HTTP_CODE="${out##*$'\n'}"
    BODY="${out%$'\n'*}"
    ETAG=$(sed -n 's/[Ee][Tt]ag: *\(.*\)\r*/\1/p' /tmp/connect-h | tr -d '\r')
}

# 1) AddBook (mutation) returns the created book as a JSON message.
rpc AddBook '{"title":"Dune","author":"Herbert","year":1965}'
[ "$HTTP_CODE" = "200" ] || fail "AddBook status $HTTP_CODE: $BODY"
echo "$BODY" | grep -q '"id":1' || fail "AddBook did not return id 1: $BODY"

# 2) GetBook (query) returns the book, content-type application/json, with ETag.
CT=$(curl -s -o /dev/null -w '%{content_type}' -X POST "$SVC/GetBook" "${hdr[@]}" -d '{"id":1}')
echo "$CT" | grep -q 'application/json' || fail "GetBook content-type not JSON: $CT"
rpc GetBook '{"id":1}'
echo "$BODY" | grep -q '"title":"Dune"' || fail "GetBook id 1 wrong: $BODY"
ET="$ETAG"
[ -n "$ET" ] || fail "cached GetBook carried no ETag"

# 3) Repeat GetBook id 1 with If-None-Match -> 304 (a cache HIT on the body key).
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SVC/GetBook" "${hdr[@]}" \
    -H "if-none-match: $ET" -d '{"id":1}')
[ "$CODE" = "304" ] || fail "expected 304 (cache hit) for repeated GetBook id 1, got $CODE"

# 4) Connect error envelope: a missing id maps to code not_found / HTTP 404.
rpc GetBook '{"id":999}'
[ "$HTTP_CODE" = "404" ] || fail "GetBook missing id status $HTTP_CODE (want 404): $BODY"
echo "$BODY" | grep -q '"code":"not_found"' || fail "GetBook missing id not a Connect not_found: $BODY"

# 5) A malformed request message maps to invalid_argument / HTTP 400.
rpc GetBook 'not json'
[ "$HTTP_CODE" = "400" ] || fail "bad body status $HTTP_CODE (want 400): $BODY"
echo "$BODY" | grep -q '"code":"invalid_argument"' || fail "bad body not invalid_argument: $BODY"

# 6) Body-variant caching: GetBook id 2 is a DISTINCT cache entry from id 1.
rpc AddBook '{"title":"Foundation","author":"Asimov","year":1951}'
rpc GetBook '{"id":2}'
echo "$BODY" | grep -q '"title":"Foundation"' || fail "GetBook id 2 wrong: $BODY"
rpc GetBook '{"id":1}'
echo "$BODY" | grep -q '"title":"Dune"' || fail "GetBook id 1 clobbered by id 2's entry: $BODY"

# 7) Mutation invalidates the cached read: cache GetBook id 1 (Dune), DeleteBook
#    id 1 (DbWrite on `books` -> purge), GetBook id 1 must now be not_found. If
#    the DbWrite hadn't purged, the cached Dune would still be served.
rpc GetBook '{"id":1}'
rpc DeleteBook '{"id":1}'
[ "$HTTP_CODE" = "200" ] || fail "DeleteBook status $HTTP_CODE: $BODY"
rpc GetBook '{"id":1}'
[ "$HTTP_CODE" = "404" ] || fail "GetBook id 1 still cached after DeleteBook (want 404): $BODY"

# 8) ListBooks reflects the mutations (cache purged by the writes above).
rpc ListBooks '{}'
echo "$BODY" | grep -q '"title":"Foundation"' || fail "ListBooks missing Foundation: $BODY"
echo "$BODY" | grep -q '"title":"Dune"' && fail "ListBooks still shows deleted Dune: $BODY"

# 9) An unknown method is a 404 (JSON-first: router not-found; full Connect
#    conformance would return an `unimplemented` envelope).
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SVC/NoSuchMethod" "${hdr[@]}" -d '{}')
[ "$CODE" = "404" ] || fail "unknown method status $CODE (want 404)"

echo "PASS: Connect unary+JSON wire shape, error envelope + status mapping,"
echo "      body-hash query caching (ETag/304, per-id variants), and mutation"
echo "      invalidation all work"
