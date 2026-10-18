#!/usr/bin/env bash
# Full test suite:
#   1. functional SQL tests (strings, rate limiting, sorted sets),
#   2. concurrent stress with invariant checks (kv.verify()),
#   3. exactness of the rate limiter under concurrency,
#   4. a second server with tiny limits for eviction and pool exhaustion.
#
#   docker compose build pg && ./test/run.sh
set -euo pipefail
export MSYS_NO_PATHCONV=1   # keep /test/... paths intact under Git Bash on Windows
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }
psql_in() { local c=$1; shift; docker exec -i "$c" psql -U postgres -X -v ON_ERROR_STOP=1 "$@"; }
pgbench_in() { local c=$1; shift; docker exec "$c" pgbench -h 127.0.0.1 -U postgres -n -M prepared "$@" postgres; }
wait_ready() { until docker exec "$1" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1; do sleep 1; done; }
bench_summary() { grep -E '^(tps|number of failed)'; }

docker compose up -d pg >/dev/null
PG=$(docker compose ps -q pg)
wait_ready "$PG"

step "functional: strings";       psql_in "$PG" -q -f /test/smoke.sql | tail -2 | head -1
step "functional: rate limiting"; psql_in "$PG" -q -f /test/rate_limit.sql | tail -2 | head -1
step "functional: sorted sets";   psql_in "$PG" -q -f /test/zset.sql 2>&1 | grep -E 'match the SQL|zset OK'

step "stress: strings + sorted sets mixed (32 clients, 20 s)"
pgbench_in "$PG" -c 32 -j 8 -T 20 -D members=2000 \
  -f /test/stress.sql@1 -f /test/zset_stress.sql@1 | bench_summary
psql_in "$PG" -At -c "SELECT 'verify: ' || kv.verify() || ' live entries, invariants OK'"

step "rate limit is exact under concurrency (16 clients, 10 s, capacity 1000 per day)"
psql_in "$PG" -q -c "SELECT kv.del('rl-conc'), kv.del('rl-conc-allowed')" >/dev/null
pgbench_in "$PG" -c 16 -j 4 -T 10 -f /test/rate_limit_concurrency.sql | bench_summary
allowed=$(psql_in "$PG" -At -c "SELECT kv.get('rl-conc-allowed')")
echo "allowed calls: $allowed"
[ "$allowed" = 1000 ] || { echo "FAIL: expected exactly 1000 allowed calls"; exit 1; }

step "small limits: pool exhaustion, eviction under load"
SMALL=pgkv-test-small
docker rm -f "$SMALL" >/dev/null 2>&1 || true
trap 'docker rm -f "$SMALL" >/dev/null 2>&1 || true' EXIT
TESTDIR=$(pwd -W 2>/dev/null || pwd)/test
docker run -d --name "$SMALL" --cpus 4 -e POSTGRES_PASSWORD=postgres -v "$TESTDIR:/test:ro" pg_kv:dev \
  postgres -c shared_preload_libraries=pg_kv -c pg_kv.max_entries=1024 -c pg_kv.shards=4 \
           -c pg_kv.max_zset_members=200 -c pg_kv.max_member_bytes=16 -c pg_kv.sweep_interval_ms=50 >/dev/null
wait_ready "$SMALL"
psql_in "$SMALL" -q -f /test/limits.sql | tail -2 | head -1
pgbench_in "$SMALL" -c 32 -j 8 -T 15 -D members=40 \
  -f /test/stress.sql@1 -f /test/zset_stress.sql@1 | bench_summary
psql_in "$SMALL" -At -c "SELECT 'verify: ' || kv.verify() || ' live entries, invariants OK'" \
                     -c "SELECT 'stats: ' || row(s.*)::text FROM kv.stats() s"

step "all tests passed"
