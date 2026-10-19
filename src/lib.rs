//! pg_kv: a Redis-style key/value cache that lives in PostgreSQL shared memory.
//!
//! Entries are not stored in tables, so there is no WAL, no MVCC row versions and
//! nothing for VACUUM to clean up. The trade-offs are deliberate and cache-like:
//! writes are not transactional, the cache is empty after a restart, and standbys
//! keep their own independent cache.
//!
//! Besides strings and counters, keys can hold token buckets (`kv.rate_limit`)
//! and sorted sets (`kv.zadd`, `kv.zrank`, ...). See `store.rs` for the layout.

mod bucket;
mod store;
mod zset;

use pgrx::bgworkers::{BackgroundWorker, BackgroundWorkerBuilder, SignalWakeFlags};
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::CStr;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use store::{
    as_bytes, hash_key, is_expired, raise, Geometry, Kv, KvError, KIND_STRING, KIND_ZSET, LIVE,
};
use PgSqlErrorCode::*;
use zset::ZsetVal;

::pgrx::pg_module_magic!(name, version);

static MAX_ENTRIES: GucSetting<i32> = GucSetting::<i32>::new(65536);
static MAX_KEY_BYTES: GucSetting<i32> = GucSetting::<i32>::new(128);
static MAX_VALUE_BYTES: GucSetting<i32> = GucSetting::<i32>::new(512);
static SHARDS: GucSetting<i32> = GucSetting::<i32>::new(64);
static SWEEP_INTERVAL_MS: GucSetting<i32> = GucSetting::<i32>::new(1000);
static MAX_ZSET_MEMBERS: GucSetting<i32> = GucSetting::<i32>::new(100_000);
static MAX_MEMBER_BYTES: GucSetting<i32> = GucSetting::<i32>::new(64);

const SHMEM_NAME: &CStr = c"pg_kv";
const PRELOAD_HINT: &str =
    "Add pg_kv to shared_preload_libraries in postgresql.conf and restart PostgreSQL.";

fn geometry() -> Geometry {
    Geometry::new(
        MAX_ENTRIES.get(),
        MAX_KEY_BYTES.get(),
        MAX_VALUE_BYTES.get(),
        SHARDS.get(),
        MAX_ZSET_MEMBERS.get(),
        MAX_MEMBER_BYTES.get(),
    )
}

/// Filled in by the shmem startup hook in the postmaster and inherited by every
/// forked backend.
static mut KV: Option<Kv> = None;

fn kv() -> Kv {
    match unsafe { KV } {
        Some(kv) => kv,
        None => raise(
            ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            "pg_kv is not loaded: its shared memory was not set up at server start",
            Some(PRELOAD_HINT),
        ),
    }
}

fn now() -> i64 {
    unsafe { pg_sys::GetCurrentTimestamp() }
}

fn interval_to_micros(ttl: &Interval) -> i64 {
    let micros = ttl.as_micros();
    if micros <= 0 {
        raise(ERRCODE_INVALID_PARAMETER_VALUE, "pg_kv: interval must be positive", None);
    }
    micros.min(i64::MAX as i128) as i64
}

/// Runs `f` on the live sorted set stored under `key` with the shard lock and the
/// sorted-set lock held in shared mode. Returns None when the key is missing.
fn zset_read<R>(key: &str, f: impl FnOnce(&Kv, &ZsetVal) -> R) -> Option<R> {
    let kv = kv();
    let key = key.as_bytes();
    if key.len() > kv.g.key_max {
        return None;
    }
    let hash = hash_key(key);
    let shard = kv.shard_of(hash);
    let now = now();
    let result: Result<Option<R>, KvError> = {
        let _g = kv.lock(shard, false);
        unsafe {
            match kv.find(shard, hash, key) {
                Some(i) if !is_expired(kv.slot(shard, i), now) => {
                    let s = kv.slot(shard, i);
                    if (*s).kind != KIND_ZSET {
                        Err(KvError::WrongType)
                    } else {
                        (*s).last_access.store(now, Relaxed);
                        let zv: ZsetVal = kv.read_val_as(s);
                        let _z = kv.zset_lock(false);
                        Ok(Some(f(&kv, &zv)))
                    }
                }
                _ => Ok(None),
            }
        }
    };
    result.unwrap_or_else(|e| e.raise())
}

/// Shared body of zadd and zincrby: creates the sorted set if needed, then adds
/// the member or updates its score with `f`.
fn zset_upsert(key: &str, member: &str, f: impl FnOnce(Option<f64>) -> f64) -> (bool, f64) {
    let kv = kv();
    let (key, member) = (key.as_bytes(), member.as_bytes());
    kv.check_sizes(key, None);
    kv.check_member(member);
    let hash = hash_key(key);
    let shard = kv.shard_of(hash);
    let now = now();
    let result: Result<(bool, f64), KvError> = {
        let _g = kv.lock(shard, true);
        unsafe {
            // Find or create the set's slot before taking the sorted-set lock:
            // making room for a new key may evict (and free) another sorted set.
            let slot = match kv.find(shard, hash, key) {
                Some(i) => {
                    let s = kv.slot(shard, i);
                    if is_expired(s, now) {
                        kv.release_value(s);
                        kv.set_expiry(shard, s, 0);
                        kv.write_val_as(s, ZsetVal::new(kv.new_zid()));
                        (*s).kind = KIND_ZSET;
                        Ok(i)
                    } else if (*s).kind != KIND_ZSET {
                        Err(KvError::WrongType)
                    } else {
                        Ok(i)
                    }
                }
                None => {
                    let zv = ZsetVal::new(kv.new_zid());
                    Ok(kv.insert_new(shard, hash, key, as_bytes(&zv), KIND_ZSET, 0, now))
                }
            };
            slot.and_then(|i| {
                let s = kv.slot(shard, i);
                let mut zv: ZsetVal = kv.read_val_as(s);
                let r = {
                    let _z = kv.zset_lock(true);
                    kv.zset_upsert(&mut zv, member, f)
                };
                kv.write_val_as(s, zv);
                (*s).last_access.store(now, Relaxed);
                if zv.count == 0 {
                    // The set was just created but the member could not be added.
                    kv.remove_at(shard, i);
                }
                r
            })
        }
    };
    result.unwrap_or_else(|e| e.raise())
}

fn rate_limit_core(key: &str, capacity: i32, per: Interval, cost: i32) -> bucket::RateLimit {
    if capacity < 1 {
        raise(ERRCODE_INVALID_PARAMETER_VALUE, "pg_kv: capacity must be at least 1", None);
    }
    if cost < 0 || cost > capacity {
        raise(
            ERRCODE_INVALID_PARAMETER_VALUE,
            format!("pg_kv: cost must be between 0 and capacity ({capacity})"),
            None,
        );
    }
    let per_us = interval_to_micros(&per);
    let kv = kv();
    let key = key.as_bytes();
    kv.check_sizes(key, None);
    let hash = hash_key(key);
    let shard = kv.shard_of(hash);
    let now = now();
    let rate_per_us = capacity as f64 / per_us as f64;
    let result = {
        let _g = kv.lock(shard, true);
        unsafe { kv.take_tokens(shard, hash, key, capacity as f64, rate_per_us, cost as f64, now) }
    };
    result.unwrap_or_else(|e| e.raise())
}

fn micros_to_ms(us: i64) -> i64 {
    (us + 999) / 1000
}

#[pg_schema]
mod kv {
    use super::*;

    // ---------------------------------------------------------------- strings

    /// Stores `value` under `key`, replacing any previous value (of any kind) and TTL.
    #[pg_extern]
    fn set(key: &str, value: &str, ttl: default!(Option<Interval>, "NULL")) {
        let kv = super::kv();
        let (key, val) = (key.as_bytes(), value.as_bytes());
        kv.check_sizes(key, Some(val));
        let now = now();
        let expire_at = match ttl {
            None => 0,
            Some(ttl) => now.saturating_add(interval_to_micros(&ttl)),
        };
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        {
            let _g = kv.lock(shard, true);
            unsafe { kv.upsert_string(shard, hash, key, val, expire_at, now) };
        }
        unsafe { (*kv.hdr).sets.fetch_add(1, Relaxed) };
    }

    /// Returns the value, or NULL when the key is missing or expired.
    #[pg_extern]
    fn get(key: &str) -> Option<String> {
        let kv = super::kv();
        let key = key.as_bytes();
        if key.len() > kv.g.key_max {
            return None;
        }
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let now = now();
        let result: Result<Option<String>, KvError> = {
            let _g = kv.lock(shard, false);
            unsafe {
                match kv.find(shard, hash, key) {
                    Some(i) if !is_expired(kv.slot(shard, i), now) => {
                        let s = kv.slot(shard, i);
                        if (*s).kind != KIND_STRING {
                            Err(KvError::WrongType)
                        } else {
                            (*s).last_access.store(now, Relaxed);
                            // String values only ever come from text arguments or integers.
                            Ok(Some(String::from_utf8_unchecked(kv.val_of(s).to_vec())))
                        }
                    }
                    _ => Ok(None),
                }
            }
        };
        let value = result.unwrap_or_else(|e| e.raise());
        let counter = unsafe {
            if value.is_some() {
                &(*kv.hdr).hits
            } else {
                &(*kv.hdr).misses
            }
        };
        counter.fetch_add(1, Relaxed);
        value
    }

    /// Atomically adds `delta` to an integer value and returns the result.
    /// A missing or expired key starts at 0. An existing TTL is kept, as in Redis.
    #[pg_extern]
    fn incr(key: &str, delta: default!(i64, 1)) -> i64 {
        let kv = super::kv();
        let key = key.as_bytes();
        kv.check_sizes(key, None);
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let now = now();
        let result: Result<i64, KvError> = {
            let _g = kv.lock(shard, true);
            unsafe {
                match kv.find(shard, hash, key) {
                    Some(i) if !is_expired(kv.slot(shard, i), now) => {
                        let s = kv.slot(shard, i);
                        if (*s).kind != KIND_STRING {
                            Err(KvError::WrongType)
                        } else {
                            let current = std::str::from_utf8(kv.val_of(s))
                                .ok()
                                .and_then(|v| v.parse::<i64>().ok());
                            match current.map(|c| c.checked_add(delta)) {
                                None => Err(KvError::NotInteger),
                                Some(None) => Err(KvError::Overflow),
                                Some(Some(next)) => {
                                    kv.write_val(s, next.to_string().as_bytes());
                                    (*s).last_access.store(now, Relaxed);
                                    Ok(next)
                                }
                            }
                        }
                    }
                    Some(i) => {
                        let s = kv.slot(shard, i);
                        kv.release_value(s);
                        kv.write_val(s, delta.to_string().as_bytes());
                        kv.set_expiry(shard, s, 0);
                        (*s).last_access.store(now, Relaxed);
                        Ok(delta)
                    }
                    None => {
                        let val = delta.to_string();
                        kv.insert_new(shard, hash, key, val.as_bytes(), KIND_STRING, 0, now);
                        Ok(delta)
                    }
                }
            }
        };
        result.unwrap_or_else(|e| e.raise())
    }

    // ------------------------------------------------------------- any kind

    /// Deletes the key. Returns true if a live entry was removed.
    #[pg_extern]
    fn del(key: &str) -> bool {
        let kv = super::kv();
        let key = key.as_bytes();
        if key.len() > kv.g.key_max {
            return false;
        }
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let now = now();
        let _g = kv.lock(shard, true);
        unsafe {
            match kv.find(shard, hash, key) {
                Some(i) => {
                    let live = !is_expired(kv.slot(shard, i), now);
                    kv.remove_at(shard, i);
                    live
                }
                None => false,
            }
        }
    }

    /// Remaining time to live in milliseconds, like Redis PTTL:
    /// -1 when the key has no TTL, -2 when it does not exist.
    #[pg_extern]
    fn ttl(key: &str) -> i64 {
        let kv = super::kv();
        let key = key.as_bytes();
        if key.len() > kv.g.key_max {
            return -2;
        }
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let now = now();
        let _g = kv.lock(shard, false);
        unsafe {
            match kv.find(shard, hash, key) {
                None => -2,
                Some(i) => {
                    let s = kv.slot(shard, i);
                    if (*s).expire_at == 0 {
                        -1
                    } else if is_expired(s, now) {
                        -2
                    } else {
                        micros_to_ms((*s).expire_at - now)
                    }
                }
            }
        }
    }

    /// Sets (or with NULL, removes) the TTL of an existing key of any kind.
    /// Returns false when the key does not exist.
    #[pg_extern]
    fn expire(key: &str, ttl: Option<Interval>) -> bool {
        let kv = super::kv();
        let key = key.as_bytes();
        if key.len() > kv.g.key_max {
            return false;
        }
        let now = now();
        let expire_at = match ttl {
            None => 0,
            Some(ttl) => now.saturating_add(interval_to_micros(&ttl)),
        };
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let _g = kv.lock(shard, true);
        unsafe {
            match kv.find(shard, hash, key) {
                Some(i) if !is_expired(kv.slot(shard, i), now) => {
                    kv.set_expiry(shard, kv.slot(shard, i), expire_at);
                    true
                }
                _ => false,
            }
        }
    }

    // ---------------------------------------------------------- rate limiting

    /// Token bucket: allows up to `capacity` calls per `per`, refilling smoothly.
    /// Returns true and takes `cost` tokens if they are available.
    #[pg_extern]
    fn rate_limit(key: &str, capacity: i32, per: Interval, cost: default!(i32, 1)) -> bool {
        rate_limit_core(key, capacity, per, cost).allowed
    }

    /// Like rate_limit, but also reports the remaining tokens, how long to wait
    /// before retrying, and when the bucket is full again (for Retry-After and
    /// X-RateLimit-* headers).
    #[pg_extern]
    fn rate_limit_info(
        key: &str,
        capacity: i32,
        per: Interval,
        cost: default!(i32, 1),
    ) -> TableIterator<
        'static,
        (
            name!(allowed, bool),
            name!(remaining, i32),
            name!(retry_after_ms, i64),
            name!(reset_after_ms, i64),
        ),
    > {
        let r = rate_limit_core(key, capacity, per, cost);
        TableIterator::once((
            r.allowed,
            r.remaining,
            micros_to_ms(r.retry_after_us),
            micros_to_ms(r.reset_after_us),
        ))
    }

    // ------------------------------------------------------------ sorted sets

    /// Adds `member` with `score`, or updates its score. Returns true if added.
    #[pg_extern]
    fn zadd(key: &str, member: &str, score: f64) -> bool {
        if score.is_nan() {
            raise(ERRCODE_INVALID_PARAMETER_VALUE, "pg_kv: score must not be NaN", None);
        }
        zset_upsert(key, member, |_| score).0
    }

    /// Adds `delta` to the member's score (a missing member starts at 0) and
    /// returns the new score.
    #[pg_extern]
    fn zincrby(key: &str, member: &str, delta: f64) -> f64 {
        if delta.is_nan() {
            raise(ERRCODE_INVALID_PARAMETER_VALUE, "pg_kv: delta must not be NaN", None);
        }
        zset_upsert(key, member, |old| old.unwrap_or(0.0) + delta).1
    }

    #[pg_extern]
    fn zscore(key: &str, member: &str) -> Option<f64> {
        zset_read(key, |kv, zv| unsafe { kv.zset_find(zv, member.as_bytes()).map(|n| kv.zscore_of(n)) })
            .flatten()
    }

    /// 0-based rank by ascending score (ties ordered by member), or NULL.
    #[pg_extern]
    fn zrank(key: &str, member: &str) -> Option<i64> {
        zset_read(key, |kv, zv| unsafe {
            kv.zset_find(zv, member.as_bytes()).map(|n| kv.zrank_of(zv.root, n) as i64)
        })
        .flatten()
    }

    /// 0-based rank by descending score (0 = highest), or NULL.
    #[pg_extern]
    fn zrevrank(key: &str, member: &str) -> Option<i64> {
        zset_read(key, |kv, zv| unsafe {
            kv.zset_find(zv, member.as_bytes())
                .map(|n| zv.count as i64 - 1 - kv.zrank_of(zv.root, n) as i64)
        })
        .flatten()
    }

    /// Number of members (0 when the key does not exist).
    #[pg_extern]
    fn zcard(key: &str) -> i64 {
        zset_read(key, |_, zv| zv.count as i64).unwrap_or(0)
    }

    /// Members by rank, Redis ZRANGE style: `start`/`stop` are inclusive 0-based
    /// ranks, negative values count from the end; `reverse` ranks by descending score.
    #[pg_extern]
    fn zrange(
        key: &str,
        start: i64,
        stop: i64,
        reverse: default!(bool, false),
    ) -> TableIterator<'static, (name!(rank, i64), name!(member, String), name!(score, f64))> {
        let rows = zset_read(key, |kv, zv| unsafe {
            let n = zv.count as i64;
            let first = if start < 0 { start + n } else { start }.max(0);
            let last = if stop < 0 { stop + n } else { stop }.min(n - 1);
            let mut rows = Vec::new();
            for r in first..=last {
                let k = if reverse { n - 1 - r } else { r };
                let node = kv.zselect(zv.root, k as u32);
                if node == zset::NIL {
                    break;
                }
                rows.push((r, kv.zmember_string(node), kv.zscore_of(node)));
            }
            rows
        })
        .unwrap_or_default();
        TableIterator::new(rows)
    }

    /// Removes `member`; an emptied sorted set is deleted, as in Redis.
    #[pg_extern]
    fn zrem(key: &str, member: &str) -> bool {
        let kv = super::kv();
        let key = key.as_bytes();
        if key.len() > kv.g.key_max {
            return false;
        }
        let hash = hash_key(key);
        let shard = kv.shard_of(hash);
        let now = now();
        let result: Result<bool, KvError> = {
            let _g = kv.lock(shard, true);
            unsafe {
                match kv.find(shard, hash, key) {
                    Some(i) if !is_expired(kv.slot(shard, i), now) => {
                        let s = kv.slot(shard, i);
                        if (*s).kind != KIND_ZSET {
                            Err(KvError::WrongType)
                        } else {
                            let mut zv: ZsetVal = kv.read_val_as(s);
                            let removed = {
                                let _z = kv.zset_lock(true);
                                kv.zset_remove(&mut zv, member.as_bytes())
                            };
                            kv.write_val_as(s, zv);
                            if zv.count == 0 {
                                kv.remove_at(shard, i);
                            }
                            Ok(removed)
                        }
                    }
                    _ => Ok(false),
                }
            }
        };
        result.unwrap_or_else(|e| e.raise())
    }

    // ----------------------------------------------------------- maintenance

    /// True when pg_kv was loaded through shared_preload_libraries, i.e. its
    /// shared memory exists and the other functions can work.
    #[pg_extern]
    fn preloaded() -> bool {
        unsafe { KV }.is_some()
    }

    // Runs last in CREATE EXTENSION. The cache is shared by every database and
    // role of the cluster, so the destructive and full-scan functions are kept
    // for the extension owner (grant them explicitly if needed), and installing
    // into a server that did not preload the library fails instead of leaving
    // functions that can only raise errors.
    extension_sql!(
        r#"
REVOKE EXECUTE ON FUNCTION kv.flush() FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION kv.verify() FROM PUBLIC;

DO $$
BEGIN
  IF NOT kv.preloaded() THEN
    RAISE EXCEPTION 'pg_kv must be loaded through shared_preload_libraries'
      USING ERRCODE = 'object_not_in_prerequisite_state',
            HINT = 'Add pg_kv to shared_preload_libraries in postgresql.conf and restart PostgreSQL.';
  END IF;
END
$$;
"#,
        name = "finalize",
        finalize,
    );

    /// Removes all entries and returns how many were removed.
    #[pg_extern]
    fn flush() -> i64 {
        let kv = super::kv();
        let mut removed = 0i64;
        for shard in 0..kv.g.nshards {
            let _g = kv.lock(shard, true);
            removed += unsafe { kv.clear_shard(shard) } as i64;
        }
        removed
    }

    /// Checks the internal invariants of every shard and sorted set and returns
    /// the number of live entries: counters match the slots, every live entry
    /// sits in its own shard and is reachable from its home slot, and every
    /// sorted set is a well-formed treap whose members are all indexed.
    /// Raises on corruption.
    #[pg_extern]
    fn verify() -> i64 {
        let kv = super::kv();
        let mut total = 0i64;
        for shard in 0..kv.g.nshards {
            let problem: Option<String> = {
                let _g = kv.lock(shard, false);
                unsafe {
                    let (mut live, mut with_ttl) = (0u32, 0u32);
                    let mut problem = None;
                    for i in 0..kv.g.slots_per_shard {
                        let s = kv.slot(shard, i);
                        if (*s).state != LIVE {
                            continue;
                        }
                        live += 1;
                        if (*s).expire_at != 0 {
                            with_ttl += 1;
                        }
                        if kv.shard_of((*s).hash) != shard || hash_key(kv.key_of(s)) != (*s).hash {
                            problem = Some(format!("slot {i} holds a key hashed to another shard"));
                            break;
                        }
                        if kv.find(shard, (*s).hash, kv.key_of(s)) != Some(i) {
                            problem = Some(format!("slot {i} is not reachable from its home slot"));
                            break;
                        }
                        if (*s).kind == KIND_ZSET {
                            let zv: ZsetVal = kv.read_val_as(s);
                            let _z = kv.zset_lock(false);
                            if let Err(e) = kv.zset_check(&zv) {
                                problem = Some(format!("sorted set in slot {i}: {e}"));
                                break;
                            }
                        }
                    }
                    let sh = kv.shard(shard);
                    total += live as i64;
                    problem.or_else(|| {
                        ((*sh).count != live || (*sh).ttl_count != with_ttl).then(|| {
                            format!(
                                "count {}/{} and ttl_count {}/{} (stored/actual)",
                                (*sh).count, live, (*sh).ttl_count, with_ttl
                            )
                        })
                    })
                }
            };
            if let Some(p) = problem {
                raise(ERRCODE_DATA_CORRUPTED, format!("pg_kv: shard {shard} is inconsistent: {p}"), None);
            }
        }
        let pool = {
            let _z = kv.zset_lock(false);
            unsafe { kv.zset_pool_check() }
        };
        if let Err(p) = pool {
            raise(ERRCODE_DATA_CORRUPTED, format!("pg_kv: sorted-set node pool is inconsistent: {p}"), None);
        }
        total
    }

    #[pg_extern]
    fn stats() -> TableIterator<
        'static,
        (
            name!(entries, i64),
            name!(capacity, i64),
            name!(hits, i64),
            name!(misses, i64),
            name!(sets, i64),
            name!(evictions, i64),
            name!(expired, i64),
            name!(zset_members, i64),
            name!(zset_capacity, i64),
            name!(shmem_bytes, i64),
        ),
    > {
        let kv = super::kv();
        let mut entries = 0i64;
        for shard in 0..kv.g.nshards {
            let _g = kv.lock(shard, false);
            entries += unsafe { (*kv.shard(shard)).count } as i64;
        }
        let zset_members = {
            let _z = kv.zset_lock(false);
            (unsafe { kv.zset_used() }) as i64
        };
        let h = unsafe { &*kv.hdr };
        TableIterator::once((
            entries,
            kv.g.cap_per_shard as i64 * kv.g.nshards as i64,
            h.hits.load(Relaxed) as i64,
            h.misses.load(Relaxed) as i64,
            h.sets.load(Relaxed) as i64,
            h.evictions.load(Relaxed) as i64,
            h.expired.load(Relaxed) as i64,
            zset_members,
            kv.g.z_nodes as i64,
            kv.g.total as i64,
        ))
    }
}

static mut PREV_SHMEM_REQUEST_HOOK: Option<unsafe extern "C-unwind" fn()> = None;
static mut PREV_SHMEM_STARTUP_HOOK: Option<unsafe extern "C-unwind" fn()> = None;

#[pg_guard]
unsafe extern "C-unwind" fn kv_shmem_request() {
    unsafe {
        if let Some(prev) = PREV_SHMEM_REQUEST_HOOK {
            pg_sys::submodules::ffi::pg_guard_ffi_boundary(|| prev());
        }
        let g = geometry();
        pg_sys::RequestAddinShmemSpace(g.total);
        pg_sys::RequestNamedLWLockTranche(SHMEM_NAME.as_ptr(), g.nlocks() as i32);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn kv_shmem_startup() {
    unsafe {
        if let Some(prev) = PREV_SHMEM_STARTUP_HOOK {
            pg_sys::submodules::ffi::pg_guard_ffi_boundary(|| prev());
        }
        let g = geometry();
        // AddinShmemInitLock, the same way pgrx's PgLwLock reaches it.
        let init_lock = &raw mut (*pg_sys::MainLWLockArray.add(21)).lock;
        pg_sys::LWLockAcquire(init_lock, pg_sys::LWLockMode::LW_EXCLUSIVE);

        let mut found = false;
        let base = pg_sys::ShmemInitStruct(SHMEM_NAME.as_ptr(), g.total, &mut found).cast::<u8>();
        let locks = pg_sys::GetNamedLWLockTranche(SHMEM_NAME.as_ptr());
        KV = Some(Kv::attach(g, base, locks, !found));

        pg_sys::LWLockRelease(init_lock);
    }
}

/// Background worker that reclaims expired entries even when nobody reads them.
#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pg_kv_sweeper_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let kv = kv();
    loop {
        let interval = Duration::from_millis(SWEEP_INTERVAL_MS.get().max(10) as u64);
        if !BackgroundWorker::wait_latch(Some(interval)) {
            break;
        }
        let now = now();
        for shard in 0..kv.g.nshards {
            let _g = kv.lock(shard, true);
            unsafe { kv.purge_expired(shard, now) };
        }
    }
}

fn define_int(
    name: &'static CStr,
    short: &'static CStr,
    long: &'static CStr,
    setting: &'static GucSetting<i32>,
    min: i32,
    max: i32,
) {
    GucRegistry::define_int_guc(name, short, long, setting, min, max, GucContext::Postmaster, GucFlags::default());
}

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    // Without preloading there is no shared memory; the SQL functions then raise
    // an error that says how to fix it.
    if unsafe { !pg_sys::process_shared_preload_libraries_in_progress } {
        return;
    }

    define_int(
        c"pg_kv.max_entries",
        c"Maximum number of keys kept in the cache.",
        c"Shared memory is sized for this many keys at server start; beyond it, keys are evicted (sampled LRU).",
        &MAX_ENTRIES,
        16,
        i32::MAX / 4,
    );
    define_int(
        c"pg_kv.max_key_bytes",
        c"Maximum key size in bytes.",
        c"Every slot reserves this many bytes for the key.",
        &MAX_KEY_BYTES,
        1,
        65535,
    );
    define_int(
        c"pg_kv.max_value_bytes",
        c"Maximum value size in bytes.",
        c"Every slot reserves this many bytes for the value.",
        &MAX_VALUE_BYTES,
        32,
        1 << 20,
    );
    define_int(
        c"pg_kv.shards",
        c"Number of independently locked shards.",
        c"More shards reduce lock contention between concurrent sessions.",
        &SHARDS,
        1,
        4096,
    );
    define_int(
        c"pg_kv.sweep_interval_ms",
        c"How often the background worker removes expired entries.",
        c"Expired entries are never returned; the sweep only reclaims their memory.",
        &SWEEP_INTERVAL_MS,
        10,
        3_600_000,
    );
    define_int(
        c"pg_kv.max_zset_members",
        c"Total sorted-set members across all sorted sets (0 disables sorted sets).",
        c"Members come from one shared node pool; zadd fails when it is full.",
        &MAX_ZSET_MEMBERS,
        0,
        i32::MAX / 4,
    );
    define_int(
        c"pg_kv.max_member_bytes",
        c"Maximum sorted-set member size in bytes.",
        c"Every pool node reserves this many bytes for the member.",
        &MAX_MEMBER_BYTES,
        1,
        65535,
    );

    unsafe {
        PREV_SHMEM_REQUEST_HOOK = pg_sys::shmem_request_hook;
        pg_sys::shmem_request_hook = Some(kv_shmem_request);
        PREV_SHMEM_STARTUP_HOOK = pg_sys::shmem_startup_hook;
        pg_sys::shmem_startup_hook = Some(kv_shmem_startup);
    }

    BackgroundWorkerBuilder::new("pg_kv sweeper")
        .set_type("pg_kv sweeper")
        .set_library("pg_kv")
        .set_function("pg_kv_sweeper_main")
        .set_restart_time(Some(Duration::from_secs(5)))
        .load();
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries='pg_kv'"]
    }
}
