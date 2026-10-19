# Changelog

## v0.1.0 (2026-10-19)

First public release: an early version for feedback, not production ready yet.

### What it does

pg_kv is a PostgreSQL extension, written in Rust with pgrx, that keeps a Redis-style keyspace in shared memory instead of tables. There is no WAL, no MVCC row versions and nothing for VACUUM to clean up.

**Strings and counters:**
- `kv.set`, `kv.get`, `kv.del`, `kv.incr`, `kv.ttl`, `kv.expire`.
- TTLs are enforced on read, and a background worker reclaims expired entries.
- When full, it evicts with sampled LRU.

**Rate limiting:**
- `kv.rate_limit` and `kv.rate_limit_info`: a token bucket that is atomic per key.
- `kv.rate_limit_info` reports `retry_after_ms` and `reset_after_ms`.
- Idle buckets expire on their own.

**Sorted sets:**
- `kv.zadd`, `kv.zincrby`, `kv.zscore`, `kv.zrank`, `kv.zrevrank`, `kv.zrange`, `kv.zrem`, `kv.zcard`.
- They are backed by a treap with subtree sizes, so rank and select-by-rank are O(log n).

**Value kinds:** as in Redis, using the wrong kind of key raises WRONGTYPE.

**Errors and checks:**
- Every error carries a SQLSTATE. For example, `42809` is WRONGTYPE and `54000` means a limit was hit.
- `kv.verify()` checks every internal invariant.

**Safe defaults:**
- `CREATE EXTENSION` refuses to install unless pg_kv is in `shared_preload_libraries`.
- `kv.flush()` and `kv.verify()` are owner-only.
- Using the cache requires `USAGE` on schema `kv`.

**Docker images:**
- PostgreSQL 15, 16, 17 and 18 on GitHub Container Registry: `ghcr.io/murat7ay/pg_kv:pg18` and similar.
- They preload pg_kv even when the container command is overridden.

### Numbers (early, from a Windows/Docker Desktop machine)

Server-side cost per operation, one statement plus commit, compared with an `UNLOGGED` table:

| Operation | pg_kv vs UNLOGGED |
|---|---|
| writes (SET, INCR) | about 5–7× cheaper |
| reads (GET) | about 3× cheaper |

- After the benchmark the UNLOGGED table held 18k dead tuples; pg_kv's memory stayed fixed.
- Redis is likely faster per network round trip. A fair Linux comparison is still to come.

### Known limitations

- **It is a cache:**
  - not transactional (writes inside a rolled-back transaction stay);
  - not durable (empty after a restart);
  - not replicated (standbys have their own cache).
- **Server setup:**
  - It needs `shared_preload_libraries`, so it is not available on managed Postgres services.
  - The keyspace is shared by every database in the cluster.
- **Storage:**
  - Values are text only.
  - Key and value slots are fixed-size and preallocated.
- **Sorted sets:** writes are serialized by a single lock.
- **PostgreSQL 19** is not supported yet.

### Tested

`test/run.sh` runs these on PostgreSQL 15.19, 16.15, 17.11 and 18.6, with no build warnings:
- functional tests;
- randomized comparison against SQL ground truth;
- concurrent stress followed by invariant checks;
- rate-limit exactness under concurrency;
- tiny-limits eviction and pool exhaustion;
- installation checks.

The runs used `ci/local.sh`.
