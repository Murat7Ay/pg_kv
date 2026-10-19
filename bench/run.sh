#!/usr/bin/env bash
# Benchmark pg_kv vs an UNLOGGED table (same Postgres server) vs Redis.
#
# - Servers get the same CPU quota (SERVER_CPUS, default 4); clients run in separate
#   containers (CLIENT_CPUS, default 3) and connect over the compose network, like
#   an application would. BENCH_ENV labels the machine in the report.
# - Every Postgres measurement is repeated ROUNDS times with the pg_kv and
#   UNLOGGED runs interleaved; the median is reported. Redis likewise.
# - bench/server_side.sql additionally measures per-operation server cost with
#   no network round trip at all.
#
#   ./bench/run.sh                         # 16 clients, 15 s per run, 3 rounds
#   CLIENTS=64 DURATION=30 ROUNDS=5 ./bench/run.sh
set -euo pipefail
export MSYS_NO_PATHCONV=1   # keep /bench/... paths intact under Git Bash on Windows

cd "$(dirname "$0")/.."
CLIENTS=${CLIENTS:-16}
SERVER_CPUS=${SERVER_CPUS:-4}
CLIENT_CPUS=${CLIENT_CPUS:-3}
export SERVER_CPUS CLIENT_CPUS
THREADS=${THREADS:-$CLIENT_CPUS}
DURATION=${DURATION:-15}
ROUNDS=${ROUNDS:-3}
REDIS_REQUESTS=${REDIS_REQUESTS:-600000}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT=bench/results/$STAMP.md
RAW=bench/results/$STAMP.raw

dc() { docker compose --profile bench "$@"; }
psql_pg() { dc exec -T pg psql -U postgres -X -q -t -A "$@"; }
median() { sort -g | awk '{ a[NR] = $1 } END { print (NR % 2) ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2 }'; }

dc up -d --renew-anon-volumes pg redis pgbench redis-bench >/dev/null
until dc exec -T pg pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1; do sleep 1; done
until dc exec -T redis redis-cli ping >/dev/null 2>&1; do sleep 1; done

echo "== setup" | tee "$RAW"
psql_pg -f /bench/setup.sql >>"$RAW" 2>&1

run_pgbench() { # $1 = script name; prints "tps latency_ms"
  local res
  res=$(dc exec -T pgbench pgbench -h pg -U postgres -n -M prepared \
        -c "$CLIENTS" -j "$THREADS" -T "$DURATION" -f "/bench/$1.sql" postgres 2>&1)
  echo "== pgbench $1" >>"$RAW"; echo "$res" >>"$RAW"
  echo "$(echo "$res" | sed -n 's/^tps = \([0-9.]*\).*/\1/p' | head -1) $(echo "$res" | sed -n 's/^latency average = \([0-9.]*\) ms/\1/p')"
}

table_stats() { # $1 = table
  psql_pg -c "SELECT pg_size_pretty(pg_total_relation_size('$1')) || ' (live ' || n_live_tup || ', dead ' || n_dead_tup
                     || ', autovacuum runs ' || autovacuum_count || ')'
              FROM pg_stat_user_tables WHERE relname = '$1'"
}

declare -A TPS LAT
for op in set get incr; do
  for store in kv unlogged; do TPS[${store}_$op]=""; LAT[${store}_$op]=""; done
  for r in $(seq 1 "$ROUNDS"); do
    for store in kv unlogged; do
      echo "== round $r: $store $op"
      read -r t l <<<"$(run_pgbench "${store}_$op")"
      TPS[${store}_$op]+="$t"$'\n'; LAT[${store}_$op]+="$l"$'\n'
    done
  done
done
UL_SET_SIZE=$(table_stats cache_unlogged)
UL_INCR_SIZE=$(table_stats counters_unlogged)
KV_STATS=$(psql_pg -c "SELECT entries || ' entries in ' || pg_size_pretty(shmem_bytes) || ' of fixed shared memory' FROM kv.stats()")

echo "== redis"
dc exec -T redis redis-cli flushall >/dev/null
dc exec -T redis-bench redis-benchmark -h redis -q -t set -n 200000 -r 100000 -d 64 >/dev/null
declare -A RTPS RLAT
for r in $(seq 1 "$ROUNDS"); do
  echo "== round $r: redis"
  res=$(dc exec -T redis-bench redis-benchmark -h redis --csv -c "$CLIENTS" --threads "$THREADS" \
        -n "$REDIS_REQUESTS" -r 100000 -d 64 -t set,get,incr)
  echo "== redis-benchmark round $r" >>"$RAW"; echo "$res" >>"$RAW"
  for op in SET GET INCR; do
    line=$(echo "$res" | tr -d '"' | awk -F, -v t="$op" '$1 == t')
    RTPS[$op]+="$(echo "$line" | cut -d, -f2)"$'\n'; RLAT[$op]+="$(echo "$line" | cut -d, -f3)"$'\n'
  done
done

echo "== server-side cost per operation"
psql_pg -f /bench/setup.sql >/dev/null 2>&1
SERVER_SIDE=$(dc exec -T pg psql -U postgres -X -q -f /bench/server_side.sql 2>&1 | grep 'ns/op' | sed 's/^/    /')
echo "$SERVER_SIDE" >>"$RAW"

PGVER=$(psql_pg -c "SHOW server_version")
REDISVER=$(dc exec -T redis redis-server --version | sed -n 's/.*v=\([^ ]*\).*/\1/p')
HOST_CPUS=$(dc exec -T pg nproc)
CPU_MODEL=$(dc exec -T pg sh -c "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2" | sed 's/^ *//')

fmt() { printf "%'.0f" "$1"; }
{
  echo "# pg_kv benchmark — $STAMP"
  echo
  echo "PostgreSQL $PGVER and Redis $REDISVER, each server limited to $SERVER_CPUS CPUs; clients in separate $CLIENT_CPUS-CPU"
  echo "containers over the compose network. Host: ${BENCH_ENV:-local machine}, $HOST_CPUS CPUs ($CPU_MODEL)."
  echo "$CLIENTS clients / $THREADS client threads,"
  echo "100k keys, 64-byte values. Median of $ROUNDS rounds (${DURATION}s per Postgres run, $REDIS_REQUESTS requests per Redis test)."
  echo
  echo "## Throughput over the network"
  echo
  echo "| Operation | pg_kv ops/s | avg ms | UNLOGGED table ops/s | avg ms | Redis ops/s | avg ms |"
  echo "|---|---:|---:|---:|---:|---:|---:|"
  for op in set get incr; do
    up=$(echo "$op" | tr a-z A-Z)
    printf '| %s | %s | %.3f | %s | %.3f | %s | %.3f |\n' "$up" \
      "$(fmt "$(printf '%s' "${TPS[kv_$op]}" | median)")" "$(printf '%s' "${LAT[kv_$op]}" | median)" \
      "$(fmt "$(printf '%s' "${TPS[unlogged_$op]}" | median)")" "$(printf '%s' "${LAT[unlogged_$op]}" | median)" \
      "$(fmt "$(printf '%s' "${RTPS[$up]}" | median)")" "$(printf '%s' "${RLAT[$up]}" | median)"
  done
  echo
  echo "## Server-side cost per operation (no network; one statement + commit each)"
  echo
  echo '```'
  echo "$SERVER_SIDE"
  echo '```'
  echo
  echo "## Storage after the runs"
  echo
  echo "- pg_kv: $KV_STATS (does not grow with updates, nothing to vacuum)"
  echo "- cache_unlogged: $UL_SET_SIZE"
  echo "- counters_unlogged: $UL_INCR_SIZE"
} | tee "$OUT"
echo
echo "Raw output: $RAW"
