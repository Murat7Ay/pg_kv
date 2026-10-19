# pg_kv

**A Redis-style key/value cache inside PostgreSQL shared memory: no WAL, no MVCC bloat, enforced TTL.**

> Status: **spike / v0.1-dev**. Not production ready.

The "just use Postgres" answer for caching today is usually an `UNLOGGED` table. That works, but every `SET` is an `UPDATE`:
- It creates a new row version.
- It touches the index.
- It leaves dead tuples behind for VACUUM.

pg_kv keeps entries in a fixed shared-memory segment instead, so updates overwrite in place and never bloat anything. Next to strings and counters it has **rate limiting** and **sorted sets** (leaderboards).

## Why pg_kv?

"Use Postgres instead of Redis" is the most common claim of the *Postgres for everything* movement: caching is the first use case on both postgresforeverything.com and postgresisenough.dev. Their recipe is an `UNLOGGED` table. That skips WAL but keeps MVCC, so a hot cache key is rewritten, re-indexed and vacuumed on every update. In [our benchmark](#benchmark-results), an UNLOGGED cache table held **18,499 dead tuples** next to its 100k live rows.

No extension filled this gap. We checked GitHub, PGXN and the Pigsty catalogue (600+ packaged extensions) on 2026-10-18:

| Existing project | What it is | Why it does not fill the gap |
|---|---|---|
| [spat](https://github.com/Florents-Tselai/spat) | Shared-memory KV store, C | 30★, alpha ("not production ready"), AGPL-3.0, no commits since 2025-06. TTL is stored but not enforced, and there is no eviction, `INCR` or sorted-set rank |
| pg_local_cache | Shared-memory row cache, RESP2 protocol | Caches table rows by primary key; not a general KV, no TTL |
| omni_shmem (Omnigres) | Named shared-memory byte arenas | Low-level building block: no TTL, eviction or atomics |
| Postgres Pro `in_memory` | In-memory FDW tables | Proprietary (Enterprise only) |
| pgmemcache, redis_fdw | Clients for external servers | You still run Redis or memcached |
| Redka | Redis API on top of SQL tables | A separate Go server, and table-backed again |

Two more capabilities had no extension at all:
- **Rate limiting:** the only shared-memory rate-limit extension found was an empty stub. Table-based token buckets bloat the same way, and `pg_qos` limits roles, not arbitrary keys such as API keys or IPs.
- **Sorted-set rank:** Postgres has no index that answers "what rank is this score?", so `count(*) WHERE score > x` is O(n).

pg_kv covers all three with one small SQL API under the permissive PostgreSQL license, which managed clouds can adopt. Its trade-offs are those of a cache, and they are spelled out [below](#semantics-read-this-before-using-it).

### Strings and counters

```sql
CREATE EXTENSION pg_kv;

SELECT kv.set('session:42', '{"user": 1}', interval '30 minutes');
SELECT kv.get('session:42');          -- '{"user": 1}'
SELECT kv.ttl('session:42');          -- remaining ms (-1 = no TTL, -2 = missing)
SELECT kv.incr('views:post:7');       -- 1, 2, 3 ...
SELECT kv.incr('views:post:7', 10);   -- atomic add
SELECT kv.expire('views:post:7', interval '1 day');
SELECT kv.del('session:42');
SELECT * FROM kv.stats();             -- entries, hits, misses, evictions, expired, zset_members, shmem_bytes ...
SELECT kv.flush();
```

### Rate limiting (token bucket)

```sql
-- 100 requests per minute per API key, refilling smoothly (bursts up to 100)
SELECT kv.rate_limit('api:' || api_key, 100, interval '1 minute');            -- true / false

-- weighted calls, and everything needed for Retry-After / X-RateLimit-* headers
SELECT * FROM kv.rate_limit_info('upload:' || user_id, 50, interval '1 hour', cost => 5);
--  allowed | remaining | retry_after_ms | reset_after_ms
```

How the limiter behaves:
- The check is atomic per key. In the test suite, 16 sessions hammered one bucket for 10 seconds and got **exactly** its capacity of allowed calls.
- An absent bucket counts as a full one. Each bucket's TTL is therefore set to the moment it would be full again, so idle buckets expire by themselves.
- It is meant for PostgREST and Supabase `db-pre-request` hooks, or any SQL-side throttling.

### Sorted sets (leaderboards)

```sql
SELECT kv.zadd('lb', 'player:9', 1520);          -- true if the member is new
SELECT kv.zincrby('lb', 'player:9', 30);         -- 1550
SELECT kv.zrevrank('lb', 'player:9');            -- 0 = top of the board, O(log n)
SELECT kv.zrank('lb', 'player:9');               -- ascending rank
SELECT kv.zscore('lb', 'player:9');              -- 1550
SELECT * FROM kv.zrange('lb', 0, 9, reverse => true);   -- top 10: rank | member | score
SELECT kv.zcard('lb');
SELECT kv.zrem('lb', 'player:9');                -- an emptied set is deleted, as in Redis
```

Ordering and ranges:
- Members are ordered by score, then by member bytes, which is Redis' tie-break.
- `zrange` takes inclusive ranks, and negative ranks count from the end, as in Redis.
- Postgres has no index that answers rank cheaply: `count(*) WHERE score > x` is O(n). Here the rank comes from a treap that keeps subtree sizes.

### Value kinds

A key holds a string, a token bucket or a sorted set:
- An operation on the wrong kind fails with `WRONGTYPE`, as in Redis.
- `kv.set` and `kv.del` work on any kind.
- `kv.ttl` and `kv.expire` work on any kind too, so a whole leaderboard can expire.

## How it works

- **Shared memory** is sized at server start.
  - Keys: `max_entries × (max_key_bytes + max_value_bytes + 40 B)`.
  - Sorted-set member pool: `max_zset_members × (max_member_bytes + 48 B)`.
- **Sharding:** the key table is split into `pg_kv.shards` shards (default 64), each with its own LWLock. Reads take a shared lock and writes an exclusive one, on that shard only.
- **Hash table per shard:** open addressing with linear probing and backward-shift deletion, so there are no tombstones. Each shard stays at most about 75% full.
- **TTL is enforced on read:** an expired key is never returned.
- **Expired-entry cleanup:** the `pg_kv sweeper` background worker reclaims expired entries every `pg_kv.sweep_interval_ms`, even if nobody reads them. That includes idle rate-limit buckets and whole sorted sets.
- **Eviction:** when a shard is full, expired entries are purged first. If it is still full, pg_kv uses sampled LRU like Redis' `maxmemory-samples`: it evicts the least recently used of 16 sampled entries. An evicted sorted set returns its members to the pool.
- **Sorted sets:**
  - The key's slot stores a small header.
  - Members live in a shared node pool, organised as a treap with subtree sizes, so insert, delete, rank and select-by-rank are all O(log n).
  - A separate open-addressing index maps (set, member) to its node.
  - Members across all sets share one LWLock. Lock order is always shard, then sorted-set lock.

## Configuration (postgresql.conf, restart required)

| GUC | Default | Meaning |
|---|---|---|
| `shared_preload_libraries` | — | must include `pg_kv` |
| `pg_kv.max_entries` | 65536 | capacity (keys of any kind) |
| `pg_kv.max_key_bytes` | 128 | max key size |
| `pg_kv.max_value_bytes` | 512 | max value size (min 32) |
| `pg_kv.shards` | 64 | lock shards |
| `pg_kv.sweep_interval_ms` | 1000 | expiry sweep period |
| `pg_kv.max_zset_members` | 100000 | total sorted-set members across all sets (0 disables sorted sets); `zadd` fails when the pool is full |
| `pg_kv.max_member_bytes` | 64 | max sorted-set member size |

## Semantics: read this before using it

- **Not transactional.** `kv.set` inside a transaction that rolls back still happened, the same as calling Redis from your app. This is intentional for a cache.
- **Not durable.** The cache is empty after a restart or crash.
- **Not replicated.** Standbys have their own, independent cache.
- **Fixed-size slots and pool.** Memory is preallocated and does not grow; oversized keys, values or members raise an error.

## Tests

```bash
docker compose build pg
./test/run.sh
PG_MAJOR=16 ./test/run.sh     # any of 15, 16, 17, 18 (builds pg_kv:16 if missing)
```

CI (`.github/workflows/ci.yml`) runs the suite against PostgreSQL 15, 16, 17 and 18 on every push and pull request. Rust and cargo-pgrx are built once in a shared stage, and the matrix jobs reuse its layer cache.

The suite has four parts:

- **Functional SQL tests** (`smoke.sql`, `rate_limit.sql`, `zset.sql`) for strings, TTL, eviction, the token bucket, sorted sets and WRONGTYPE. Sorted sets are also run through 30k random `zadd`/`zincrby`/`zrem` operations, and every member's score and rank is compared against a plain SQL table used as ground truth.
- **Concurrent stress:** 32 clients mix string and sorted-set operations, including whole-set deletes and expiry. Afterwards `kv.verify()` checks every invariant:
  - shard counters match the slots;
  - every key is reachable;
  - every sorted set is a well-formed treap whose members are all indexed;
  - the node pool adds up.
- **Rate-limit exactness:** 16 clients hit one bucket and must get exactly its capacity.
- **Tiny-limits server** (1024 keys, 200 sorted-set members) covers:
  - pool exhaustion, without leaving an empty set behind;
  - reuse of freed nodes;
  - eviction freeing sorted-set members;
  - the same stress run under constant eviction.

## Benchmark

```bash
./bench/run.sh            # pg_kv vs UNLOGGED table vs Redis, results in bench/results/
SERVER_CPUS=2 CLIENT_CPUS=2 ROUNDS=5 ./bench/run.sh
```

How the benchmark is set up:
- Each server gets the same CPU quota (`SERVER_CPUS`).
- The clients (`pgbench`, `redis-benchmark`) run in separate containers with `CLIENT_CPUS` and connect over the network.
- Only one server is under load at a time.

The **Benchmark** workflow (`.github/workflows/bench.yml`, started by hand) runs the same script on a Linux GitHub runner. That avoids the Docker Desktop networking that made the Redis numbers below inconclusive. It writes the report to the job summary.

## Benchmark results (spike, 2026-10-19)

**Caveat:** this machine runs Docker Desktop on Windows (WSL2), with other containers also running on it. The numbers are noisy and the absolute values do not carry over to anywhere else. These comparisons are reliable:
- the **server-side cost** below, which involves no network;
- the **pg_kv vs UNLOGGED** throughput, measured interleaved within the same rounds.

The Redis comparison is **not conclusive** yet. Through Docker Desktop's network, every operation, Redis included, tops out around 10–17k ops/s, so the network is what is being measured. That comparison has to be re-run on Linux.

### Server-side cost per operation

Measured with no network round trip: one statement plus a commit per operation, 100k operations each. The range covers 3 runs.

| Operation | pg_kv | UNLOGGED table | pg_kv is |
|---|---:|---:|---|
| SET (upsert) | 10–21 µs | 53–149 µs | **~5–7× cheaper** |
| GET | 10–13 µs | 26–44 µs | **~3× cheaper** |
| INCR | 8–13 µs | 58–69 µs | **~5–7× cheaper** |

### Throughput over the network

Setup: 16 clients, each server limited to 4 CPUs, clients in separate containers. Values are the median of 3 interleaved rounds (`bench/results/20261019-012615.md`).

| Operation | pg_kv ops/s | UNLOGGED ops/s | pg_kv / UNLOGGED | Redis ops/s (inconclusive here) |
|---|---:|---:|---:|---:|
| SET | 22,186 | 14,040 | 1.6× | 10,552 |
| GET | 16,832 | 10,657 | 1.6× | 13,557 |
| INCR | 10,154 | 5,286 | 1.9× | 13,641 |

### Bloat

After the runs:
- the UNLOGGED cache table held **18,499 dead tuples** next to its 100,000 live rows and waits for autovacuum;
- the counters table held 5,439 dead tuples.

pg_kv used the same 108 MB of preallocated shared memory before, during and after. It creates no dead tuples, needs no VACUUM and writes no WAL.

## Known limitations / next steps

- **Fixed-size slots waste memory** on small values. 200k entries × (128 B key + 256 B value) uses 108 MB. Variable-size storage (slab classes) is next.
- **Values are text only.** `bytea` and `jsonb` overloads are missing.
- **Requires `shared_preload_libraries`.** A no-preload mode built on PG17's DSM registry is possible.
- **Tested on PostgreSQL 15, 16, 17 and 18** (CI matrix). PostgreSQL 19 is not covered yet; it changes some shared-memory and index-AM APIs.
- **One lock for all sorted sets.** Sorted-set writes are serialized; reads run in parallel. Striping the pool per set is the next step if contention shows up.
- **No benchmarks yet for rate limiting and sorted sets.** These also still need a fair Linux benchmark against Redis.

## License

[PostgreSQL License](LICENSE) © 2026 Murat Ay.
