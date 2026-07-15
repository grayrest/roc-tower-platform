#!/usr/bin/env bash
# End-to-end wrk benchmark driver for the RealWorld example.
#
# Runs a fixed set of read endpoints against one or more pre-built server
# binaries and prints `label endpoint rps p50` rows. Used to compare host
# variants (e.g. the pre-arena baseline vs the request-arena branch) on a
# byte-identical seeded database.
#
# Usage:
#   bench/run.sh <label:binary> [<label:binary> ...]
# Example:
#   python3 bench/seed.py bench/seeded.db          # once; deterministic
#   just build examples/realworld/main.roc && cp main bench/srv-arena
#   git worktree add /tmp/tp-main main && (cd /tmp/tp-main && just build examples/realworld/main.roc) \
#     && cp /tmp/tp-main/main bench/srv-main
#   bench/run.sh main:bench/srv-main arena:bench/srv-arena
#
# Requires: wrk, curl. The seeded DB (bench/seeded.db) must exist.
set -uo pipefail
cd "$(dirname "$0")/.."

SEED_DB=bench/seeded.db
DBPATH=/tmp/tower-platform-realworld.db
BASE_PORT=8910

if [[ ! -f "$SEED_DB" ]]; then
    echo "missing $SEED_DB — run: python3 bench/seed.py $SEED_DB" >&2
    exit 1
fi

# label|url-path — a flat single-row read, a slug read, a profile, a large
# multi-row list, and a multi-row comments document. The list and comments
# endpoints are the @sql json multi-row shapes the arena is expected to help.
declare -a EPS=(
    "byslug|/api/articles/article-2785"
    "profile|/api/profiles/user14"
    "list|/api/articles?limit=20"
    "comments|/api/articles/article-2785/comments"
    "tags|/api/tags"
)

start() { # binary port -> pid
    rm -f "$DBPATH" "$DBPATH"-wal "$DBPATH"-shm
    cp "$SEED_DB" "$DBPATH"
    PORT="$2" "$1" >"/tmp/bench-$2.log" 2>&1 &
    local pid=$!
    for _ in $(seq 1 80); do
        curl -s "http://127.0.0.1:$2/api/tags" >/dev/null 2>&1 && { echo "$pid"; return; }
        sleep 0.25
    done
    echo "server $1 did not come up on port $2" >&2
    kill -9 "$pid" 2>/dev/null
    return 1
}

bench() { # port path -> "rps p50"
    wrk -t2 -c8 -d3s "http://127.0.0.1:$1$2" >/dev/null 2>&1        # warm
    local out
    out=$(wrk -t4 -c16 -d15s --latency "http://127.0.0.1:$1$2" 2>/dev/null)
    echo "$(awk '/Requests\/sec/{print $2}' <<<"$out") $(awk '/^ *50%/{print $2}' <<<"$out")"
}

echo "# server endpoint rps p50"
port=$BASE_PORT
for spec in "$@"; do
    label="${spec%%:*}"
    binary="${spec#*:}"
    port=$((port + 1))
    pid=$(start "$binary" "$port") || exit 1
    for ep in "${EPS[@]}"; do
        echo "$label ${ep%%|*} $(bench "$port" "${ep##*|}")"
    done
    kill -9 "$pid" 2>/dev/null
    sleep 1
done
