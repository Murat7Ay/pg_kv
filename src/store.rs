//! Shared-memory layout and the sharded open-addressing hash table.
//!
//! The segment holds a header, per-shard bookkeeping, the hash-table slots and
//! the sorted-set region (see `zset.rs`). Each shard is an open-addressing table
//! with linear probing and backward-shift deletion (no tombstones), kept at most
//! ~75% full. Every slot has a fixed size of `max_key_bytes + max_value_bytes`
//! plus a header, and a `kind` that says how its value bytes are interpreted.

use pgrx::pg_sys;
use pgrx::pg_sys::panic::ErrorReport;
use pgrx::prelude::*;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

use crate::zset::{Node, ZHeader};

pub const MAGIC: u64 = 0x7067_6b76_0000_0002;
/// Live entries inspected per eviction (sampled LRU, like Redis' maxmemory-samples).
const EVICTION_SAMPLES: usize = 16;

pub const EMPTY: u32 = 0;
pub const LIVE: u32 = 1;

/// Value kinds. As in Redis, an operation on a key holding another kind fails
/// with WRONGTYPE, while `set` and `del` work on any kind.
pub const KIND_STRING: u32 = 0;
pub const KIND_BUCKET: u32 = 1;
pub const KIND_ZSET: u32 = 2;

#[repr(C)]
pub struct Header {
    pub magic: u64,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub sets: AtomicU64,
    pub evictions: AtomicU64,
    pub expired: AtomicU64,
}

/// Per-shard bookkeeping, protected by the shard's LWLock.
#[repr(C)]
pub struct Shard {
    pub count: u32,
    /// Live entries that have a TTL; lets purges skip shards with nothing to expire.
    pub ttl_count: u32,
    /// Where the next eviction sample starts, so evictions rotate through the shard.
    pub hand: u32,
}

/// Slot header; the key bytes follow it, then the value bytes at `key_max`.
#[repr(C)]
pub struct Slot {
    pub state: u32,
    pub key_len: u32,
    pub val_len: u32,
    pub kind: u32,
    pub hash: u64,
    /// Absolute expiry as a TimestampTz (microseconds since 2000-01-01); 0 = never.
    pub expire_at: i64,
    /// Last read or write. Readers update it under a shared lock, hence atomic.
    pub last_access: AtomicI64,
}

#[derive(Clone, Copy)]
pub struct Geometry {
    pub nshards: u32,
    pub slots_per_shard: u32,
    pub cap_per_shard: u32,
    pub key_max: usize,
    pub val_max: usize,
    pub slot_size: usize,
    pub shards_off: usize,
    pub slots_off: usize,
    /// Sorted-set node pool size; 0 disables sorted sets.
    pub z_nodes: u32,
    pub member_max: usize,
    pub node_size: usize,
    pub z_index_slots: u32,
    pub z_hdr_off: usize,
    pub z_nodes_off: usize,
    pub z_index_off: usize,
    pub total: usize,
}

impl Geometry {
    pub fn new(
        max_entries: i32,
        key_max: i32,
        val_max: i32,
        shards: i32,
        zset_members: i32,
        member_max: i32,
    ) -> Self {
        let nshards = shards.max(1) as u32;
        let max_entries = max_entries.max(1) as u64;
        let cap_per_shard = max_entries.div_ceil(nshards as u64).max(1) as u32;
        // Stay at most ~75% full so probes are short, and guarantee at least one
        // empty slot per shard so every probe sequence terminates.
        let slots_per_shard = (cap_per_shard as u64 * 4).div_ceil(3) as u32 + 1;
        let key_max = key_max as usize;
        let val_max = val_max as usize;
        let slot_size = align_up(size_of::<Slot>() + key_max + val_max, 8);
        let shards_off = align_up(size_of::<Header>(), 64);
        let slots_off = align_up(shards_off + nshards as usize * size_of::<Shard>(), 64);

        let z_nodes = zset_members.max(0) as u32;
        let member_max = member_max as usize;
        let node_size = align_up(size_of::<Node>() + member_max, 8);
        let z_index_slots = if z_nodes == 0 { 0 } else { (z_nodes as u64 * 4).div_ceil(3) as u32 + 1 };
        let z_hdr_off = align_up(slots_off + nshards as usize * slots_per_shard as usize * slot_size, 64);
        let z_nodes_off = align_up(z_hdr_off + size_of::<ZHeader>(), 64);
        let z_index_off = align_up(z_nodes_off + z_nodes as usize * node_size, 64);
        let total = z_index_off + z_index_slots as usize * size_of::<u32>();
        Geometry {
            nshards,
            slots_per_shard,
            cap_per_shard,
            key_max,
            val_max,
            slot_size,
            shards_off,
            slots_off,
            z_nodes,
            member_max,
            node_size,
            z_index_slots,
            z_hdr_off,
            z_nodes_off,
            z_index_off,
            total,
        }
    }

    /// LWLocks in the tranche: one per shard plus one for the sorted-set region.
    pub fn nlocks(&self) -> u32 {
        self.nshards + 1
    }
}

pub const fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// FNV-1a over all parts, followed by the splitmix64 finalizer to spread FNV's
/// weak low bits.
pub fn hash_parts(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

pub fn hash_key(key: &[u8]) -> u64 {
    hash_parts(&[key])
}

/// The raw bytes of a plain-old-data value, for storing it in a slot.
pub fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

pub unsafe fn is_expired(s: *mut Slot, now: i64) -> bool {
    (*s).expire_at != 0 && (*s).expire_at <= now
}

/// Raises an ERROR with a specific SQLSTATE, so clients can tell a WRONGTYPE
/// from a full pool without parsing message text.
pub fn raise(code: PgSqlErrorCode, message: impl Into<String>, hint: Option<&str>) -> ! {
    let mut report = ErrorReport::new(code, message.into(), function_name!());
    if let Some(hint) = hint {
        report = report.set_hint(hint);
    }
    report.report(PgLogLevel::ERROR);
    unreachable!()
}

/// Errors detected while a lock is held; raised only after the locks are released.
pub enum KvError {
    WrongType,
    NotInteger,
    Overflow,
    PoolFull,
    NotANumber,
}

impl KvError {
    pub fn raise(self) -> ! {
        use PgSqlErrorCode::*;
        match self {
            KvError::WrongType => raise(
                ERRCODE_WRONG_OBJECT_TYPE,
                "pg_kv: WRONGTYPE operation against a key holding the wrong kind of value",
                None,
            ),
            KvError::NotInteger => raise(ERRCODE_INVALID_TEXT_REPRESENTATION, "pg_kv: value is not an integer", None),
            KvError::Overflow => raise(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, "pg_kv: increment would overflow", None),
            KvError::PoolFull => raise(
                ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                "pg_kv: the sorted-set member pool is full",
                Some("Raise pg_kv.max_zset_members (requires a restart) or remove members."),
            ),
            KvError::NotANumber => {
                raise(ERRCODE_INVALID_PARAMETER_VALUE, "pg_kv: resulting score is not a number", None)
            }
        }
    }
}

/// Releases an LWLock on drop. Mirrors pgrx's PgLwLock: if we are unwinding from
/// a Postgres ERROR the lock is released by transaction abort instead.
pub struct LockGuard(*mut pg_sys::LWLock);

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe {
            if pg_sys::InterruptHoldoffCount > 0 {
                pg_sys::LWLockRelease(self.0);
            }
        }
    }
}

/// Process-local view of the shared segment. Lock order is always a shard lock
/// first, then the sorted-set lock; nothing takes a shard lock while holding the
/// sorted-set lock.
#[derive(Clone, Copy)]
pub struct Kv {
    pub g: Geometry,
    pub hdr: *mut Header,
    pub(crate) shards: *mut Shard,
    pub(crate) slots: *mut u8,
    pub(crate) zhdr: *mut ZHeader,
    pub(crate) znodes: *mut u8,
    pub(crate) zindex: *mut u32,
    pub(crate) locks: *mut pg_sys::LWLockPadded,
}

impl Kv {
    /// Builds the view over the mapped segment, initializing it when `fresh`.
    pub unsafe fn attach(g: Geometry, base: *mut u8, locks: *mut pg_sys::LWLockPadded, fresh: bool) -> Kv {
        let kv = Kv {
            g,
            hdr: base.cast(),
            shards: base.add(g.shards_off).cast(),
            slots: base.add(g.slots_off),
            zhdr: base.add(g.z_hdr_off).cast(),
            znodes: base.add(g.z_nodes_off),
            zindex: base.add(g.z_index_off).cast(),
            locks,
        };
        if fresh {
            ptr::write_bytes(base, 0, g.total);
            (*kv.hdr).magic = MAGIC;
            kv.zset_init();
        }
        kv
    }

    fn lock_n(&self, n: u32, exclusive: bool) -> LockGuard {
        let mode = if exclusive {
            pg_sys::LWLockMode::LW_EXCLUSIVE
        } else {
            pg_sys::LWLockMode::LW_SHARED
        };
        unsafe {
            let lock = &raw mut (*self.locks.add(n as usize)).lock;
            pg_sys::LWLockAcquire(lock, mode);
            LockGuard(lock)
        }
    }

    pub fn lock(&self, shard: u32, exclusive: bool) -> LockGuard {
        self.lock_n(shard, exclusive)
    }

    pub fn zset_lock(&self, exclusive: bool) -> LockGuard {
        self.lock_n(self.g.nshards, exclusive)
    }

    pub fn check_sizes(&self, key: &[u8], val: Option<&[u8]>) {
        if key.len() > self.g.key_max {
            raise(
                PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                format!("pg_kv: key is {} bytes, the limit is {} (pg_kv.max_key_bytes)", key.len(), self.g.key_max),
                None,
            );
        }
        if let Some(val) = val {
            if val.len() > self.g.val_max {
                raise(
                    PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "pg_kv: value is {} bytes, the limit is {} (pg_kv.max_value_bytes)",
                        val.len(),
                        self.g.val_max
                    ),
                    None,
                );
            }
        }
    }

    pub fn shard_of(&self, hash: u64) -> u32 {
        ((hash >> 32) % self.g.nshards as u64) as u32
    }

    fn home(&self, hash: u64) -> u32 {
        ((hash & 0xffff_ffff) % self.g.slots_per_shard as u64) as u32
    }

    fn next(&self, i: u32) -> u32 {
        if i + 1 == self.g.slots_per_shard {
            0
        } else {
            i + 1
        }
    }

    pub unsafe fn shard(&self, shard: u32) -> *mut Shard {
        self.shards.add(shard as usize)
    }

    pub unsafe fn slot(&self, shard: u32, idx: u32) -> *mut Slot {
        let i = shard as usize * self.g.slots_per_shard as usize + idx as usize;
        self.slots.add(i * self.g.slot_size).cast()
    }

    pub unsafe fn key_of<'a>(&self, s: *mut Slot) -> &'a [u8] {
        std::slice::from_raw_parts((s as *const u8).add(size_of::<Slot>()), (*s).key_len as usize)
    }

    unsafe fn val_ptr(&self, s: *mut Slot) -> *mut u8 {
        (s as *mut u8).add(size_of::<Slot>() + self.g.key_max)
    }

    pub unsafe fn val_of<'a>(&self, s: *mut Slot) -> &'a [u8] {
        std::slice::from_raw_parts(self.val_ptr(s), (*s).val_len as usize)
    }

    pub unsafe fn write_val(&self, s: *mut Slot, val: &[u8]) {
        ptr::copy_nonoverlapping(val.as_ptr(), self.val_ptr(s), val.len());
        (*s).val_len = val.len() as u32;
    }

    /// Reads a fixed-size value (bucket state, sorted-set header). Value bytes
    /// start at an arbitrary offset, hence the unaligned access.
    pub unsafe fn read_val_as<T: Copy>(&self, s: *mut Slot) -> T {
        ptr::read_unaligned(self.val_ptr(s).cast::<T>())
    }

    pub unsafe fn write_val_as<T: Copy>(&self, s: *mut Slot, v: T) {
        self.write_val(s, as_bytes(&v));
    }

    /// Changes a live slot's expiry and keeps the shard's `ttl_count` in step.
    pub unsafe fn set_expiry(&self, shard: u32, s: *mut Slot, expire_at: i64) {
        let sh = self.shard(shard);
        match ((*s).expire_at != 0, expire_at != 0) {
            (false, true) => (*sh).ttl_count += 1,
            (true, false) => (*sh).ttl_count -= 1,
            _ => {}
        }
        (*s).expire_at = expire_at;
    }

    /// Frees whatever the slot's value owns outside the slot (sorted-set nodes)
    /// and turns it back into a plain string slot. Needs the shard's exclusive
    /// lock and takes the sorted-set lock, so callers must not hold that one
    /// unless they cleared `kind` first.
    pub unsafe fn release_value(&self, s: *mut Slot) {
        if (*s).kind == KIND_ZSET {
            let _z = self.zset_lock(true);
            self.zset_free_locked(s);
        }
        (*s).kind = KIND_STRING;
    }

    unsafe fn copy_slot(&self, src: *mut Slot, dst: *mut Slot) {
        let hdr = size_of::<Slot>();
        let key_len = (*src).key_len as usize;
        let val_len = (*src).val_len as usize;
        ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, hdr + key_len);
        ptr::copy_nonoverlapping(
            (src as *const u8).add(hdr + self.g.key_max),
            (dst as *mut u8).add(hdr + self.g.key_max),
            val_len,
        );
    }

    /// Needs at least a shared lock on `shard`.
    pub unsafe fn find(&self, shard: u32, hash: u64, key: &[u8]) -> Option<u32> {
        let mut i = self.home(hash);
        loop {
            let s = self.slot(shard, i);
            if (*s).state == EMPTY {
                return None;
            }
            if (*s).hash == hash && self.key_of(s) == key {
                return Some(i);
            }
            i = self.next(i);
        }
    }

    /// Removes the entry at `idx` with backward-shift deletion: later entries of
    /// the probe run are pulled into the hole so lookups never need tombstones.
    /// Needs the exclusive lock.
    pub unsafe fn remove_at(&self, shard: u32, idx: u32) {
        let n = self.g.slots_per_shard;
        let victim = self.slot(shard, idx);
        self.release_value(victim);
        if (*victim).expire_at != 0 {
            (*self.shard(shard)).ttl_count -= 1;
        }
        let mut hole = idx;
        let mut j = idx;
        loop {
            j = self.next(j);
            let sj = self.slot(shard, j);
            if (*sj).state == EMPTY {
                break;
            }
            let home = self.home((*sj).hash);
            let d_home = (j + n - home) % n;
            let d_hole = (j + n - hole) % n;
            if d_home >= d_hole {
                self.copy_slot(sj, self.slot(shard, hole));
                hole = j;
            }
        }
        (*self.slot(shard, hole)).state = EMPTY;
        (*self.shard(shard)).count -= 1;
    }

    /// Removes every expired entry of the shard. Needs the exclusive lock.
    pub unsafe fn purge_expired(&self, shard: u32, now: i64) -> u64 {
        if (*self.shard(shard)).ttl_count == 0 {
            return 0;
        }
        let mut removed = 0;
        let mut i = 0;
        while i < self.g.slots_per_shard {
            let s = self.slot(shard, i);
            if (*s).state == LIVE && is_expired(s, now) {
                // remove_at may shift a later entry into slot i, so re-check it.
                self.remove_at(shard, i);
                removed += 1;
            } else {
                i += 1;
            }
        }
        if removed > 0 {
            (*self.hdr).expired.fetch_add(removed, Relaxed);
        }
        removed
    }

    /// Sampled LRU: evict the least recently used of the next few live entries.
    unsafe fn evict_one(&self, shard: u32) {
        let sh = self.shard(shard);
        let mut i = (*sh).hand % self.g.slots_per_shard;
        let mut victim: Option<(u32, i64)> = None;
        let mut sampled = 0;
        for _ in 0..self.g.slots_per_shard {
            let s = self.slot(shard, i);
            if (*s).state == LIVE {
                let t = (*s).last_access.load(Relaxed);
                if victim.map_or(true, |(_, vt)| t < vt) {
                    victim = Some((i, t));
                }
                sampled += 1;
                if sampled == EVICTION_SAMPLES {
                    break;
                }
            }
            i = self.next(i);
        }
        (*sh).hand = i;
        if let Some((idx, _)) = victim {
            self.remove_at(shard, idx);
            (*self.hdr).evictions.fetch_add(1, Relaxed);
        }
    }

    /// Inserts a key known to be absent and returns its slot index. Needs the
    /// shard's exclusive lock and must not be called with the sorted-set lock
    /// held, since making room may free an evicted sorted set.
    pub unsafe fn insert_new(
        &self,
        shard: u32,
        hash: u64,
        key: &[u8],
        val: &[u8],
        kind: u32,
        expire_at: i64,
        now: i64,
    ) -> u32 {
        let sh = self.shard(shard);
        if (*sh).count >= self.g.cap_per_shard {
            self.purge_expired(shard, now);
        }
        if (*sh).count >= self.g.cap_per_shard {
            self.evict_one(shard);
        }
        let mut i = self.home(hash);
        while (*self.slot(shard, i)).state != EMPTY {
            i = self.next(i);
        }
        let s = self.slot(shard, i);
        (*s).state = LIVE;
        (*s).kind = kind;
        (*s).key_len = key.len() as u32;
        (*s).hash = hash;
        (*s).expire_at = 0; // the slot may hold a stale expiry from its previous entry
        self.set_expiry(shard, s, expire_at);
        (*s).last_access.store(now, Relaxed);
        ptr::copy_nonoverlapping(key.as_ptr(), (s as *mut u8).add(size_of::<Slot>()), key.len());
        self.write_val(s, val);
        (*sh).count += 1;
        i
    }

    /// `set`: stores a string, replacing a value of any kind and its TTL.
    pub unsafe fn upsert_string(&self, shard: u32, hash: u64, key: &[u8], val: &[u8], expire_at: i64, now: i64) {
        match self.find(shard, hash, key) {
            Some(i) => {
                let s = self.slot(shard, i);
                self.release_value(s);
                self.write_val(s, val);
                self.set_expiry(shard, s, expire_at);
                (*s).last_access.store(now, Relaxed);
            }
            None => {
                self.insert_new(shard, hash, key, val, KIND_STRING, expire_at, now);
            }
        }
    }

    /// Empties the shard, freeing any sorted sets in it. Needs the exclusive lock.
    pub unsafe fn clear_shard(&self, shard: u32) -> u32 {
        for i in 0..self.g.slots_per_shard {
            let s = self.slot(shard, i);
            if (*s).state == LIVE {
                self.release_value(s);
                (*s).state = EMPTY;
            }
        }
        let sh = self.shard(shard);
        let removed = (*sh).count;
        (*sh).count = 0;
        (*sh).ttl_count = 0;
        removed
    }
}
