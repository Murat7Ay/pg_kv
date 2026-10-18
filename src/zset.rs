//! Sorted sets, Redis ZSET style.
//!
//! A sorted-set key lives in the main hash table like any other key; its slot
//! value is a small header (`ZsetVal`) pointing at a treap whose nodes come from
//! one shared node pool. The treap keeps subtree sizes, so insert, delete, rank
//! and select-by-rank are all O(log n). Members are found through a separate
//! open-addressing index over (sorted set id, member).
//!
//! Members are ordered by (score, member bytes), the same tie-break as Redis.
//! Everything that touches nodes or the index needs the sorted-set LWLock.

use crate::store::{hash_parts, KvError, Kv, Slot};
use std::cmp::Ordering;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const NIL: u32 = u32::MAX;

#[repr(C)]
pub struct ZHeader {
    free_head: u32,
    used: u32,
    /// xorshift64 state for treap priorities.
    rng: u64,
    /// Source of sorted-set ids; atomic so ids can be taken without the lock.
    next_zid: AtomicU64,
}

/// Pool node; the member bytes follow it. Free nodes are chained through `left`.
#[repr(C)]
pub struct Node {
    zid: u64,
    hash: u64,
    score: f64,
    left: u32,
    right: u32,
    size: u32,
    prio: u32,
    member_len: u32,
    in_use: u32,
}

/// What a sorted-set key stores in its slot's value bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ZsetVal {
    pub zid: u64,
    pub root: u32,
    pub count: u32,
}

impl ZsetVal {
    pub fn new(zid: u64) -> Self {
        ZsetVal { zid, root: NIL, count: 0 }
    }
}

fn member_hash(zid: u64, member: &[u8]) -> u64 {
    hash_parts(&[&zid.to_le_bytes(), member])
}

impl Kv {
    pub(crate) unsafe fn zset_init(&self) {
        let zh = self.zhdr;
        (*zh).rng = 0x9e37_79b9_7f4a_7c15;
        (*zh).used = 0;
        (*zh).free_head = if self.g.z_nodes == 0 { NIL } else { 0 };
        for i in 0..self.g.z_nodes {
            let nd = self.node(i);
            (*nd).in_use = 0;
            (*nd).left = if i + 1 == self.g.z_nodes { NIL } else { i + 1 };
        }
        ptr::write_bytes(self.zindex.cast::<u8>(), 0xff, self.g.z_index_slots as usize * size_of::<u32>());
    }

    pub fn zset_enabled(&self) -> bool {
        self.g.z_nodes > 0
    }

    pub fn new_zid(&self) -> u64 {
        unsafe { (*self.zhdr).next_zid.fetch_add(1, Relaxed) + 1 }
    }

    pub fn check_member(&self, member: &[u8]) {
        if !self.zset_enabled() {
            pgrx::error!("pg_kv: sorted sets are disabled (pg_kv.max_zset_members = 0)");
        }
        if member.len() > self.g.member_max {
            pgrx::error!(
                "pg_kv: member is {} bytes, the limit is {} (pg_kv.max_member_bytes)",
                member.len(),
                self.g.member_max
            );
        }
    }

    /// Nodes in use. Needs the sorted-set lock.
    pub unsafe fn zset_used(&self) -> u32 {
        (*self.zhdr).used
    }

    unsafe fn node(&self, n: u32) -> *mut Node {
        self.znodes.add(n as usize * self.g.node_size).cast()
    }

    unsafe fn member_of<'a>(&self, n: u32) -> &'a [u8] {
        let nd = self.node(n);
        std::slice::from_raw_parts((nd as *const u8).add(size_of::<Node>()), (*nd).member_len as usize)
    }

    pub unsafe fn zmember_string(&self, n: u32) -> String {
        // Members only ever come from text arguments.
        String::from_utf8_unchecked(self.member_of(n).to_vec())
    }

    pub unsafe fn zscore_of(&self, n: u32) -> f64 {
        (*self.node(n)).score
    }

    // ---- node pool ----

    unsafe fn znode_alloc(&self, zid: u64, hash: u64, member: &[u8], score: f64) -> Option<u32> {
        let zh = self.zhdr;
        let n = (*zh).free_head;
        if n == NIL {
            return None;
        }
        let nd = self.node(n);
        (*zh).free_head = (*nd).left;
        (*zh).used += 1;
        let mut x = (*zh).rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (*zh).rng = x;
        ptr::write(
            nd,
            Node {
                zid,
                hash,
                score,
                left: NIL,
                right: NIL,
                size: 1,
                prio: (x >> 32) as u32,
                member_len: member.len() as u32,
                in_use: 1,
            },
        );
        ptr::copy_nonoverlapping(member.as_ptr(), (nd as *mut u8).add(size_of::<Node>()), member.len());
        Some(n)
    }

    unsafe fn znode_free(&self, n: u32) {
        let nd = self.node(n);
        (*nd).in_use = 0;
        (*nd).left = (*self.zhdr).free_head;
        (*self.zhdr).free_head = n;
        (*self.zhdr).used -= 1;
    }

    // ---- member index: (zid, member) -> node, linear probing, backward-shift delete ----

    fn zidx_home(&self, hash: u64) -> u32 {
        (hash % self.g.z_index_slots as u64) as u32
    }

    fn zidx_next(&self, p: u32) -> u32 {
        if p + 1 == self.g.z_index_slots {
            0
        } else {
            p + 1
        }
    }

    /// Returns (index position, node).
    unsafe fn zidx_find(&self, zid: u64, hash: u64, member: &[u8]) -> Option<(u32, u32)> {
        let mut p = self.zidx_home(hash);
        loop {
            let n = *self.zindex.add(p as usize);
            if n == NIL {
                return None;
            }
            let nd = self.node(n);
            if (*nd).hash == hash && (*nd).zid == zid && self.member_of(n) == member {
                return Some((p, n));
            }
            p = self.zidx_next(p);
        }
    }

    unsafe fn zidx_insert(&self, hash: u64, n: u32) {
        let mut p = self.zidx_home(hash);
        while *self.zindex.add(p as usize) != NIL {
            p = self.zidx_next(p);
        }
        *self.zindex.add(p as usize) = n;
    }

    unsafe fn zidx_remove_at(&self, pos: u32) {
        let m = self.g.z_index_slots;
        let mut hole = pos;
        let mut j = pos;
        loop {
            j = self.zidx_next(j);
            let n = *self.zindex.add(j as usize);
            if n == NIL {
                break;
            }
            let home = self.zidx_home((*self.node(n)).hash);
            if (j + m - home) % m >= (j + m - hole) % m {
                *self.zindex.add(hole as usize) = n;
                hole = j;
            }
        }
        *self.zindex.add(hole as usize) = NIL;
    }

    // ---- treap with subtree sizes ----

    unsafe fn zsize(&self, t: u32) -> u32 {
        if t == NIL {
            0
        } else {
            (*self.node(t)).size
        }
    }

    unsafe fn zfix(&self, t: u32) {
        let nd = self.node(t);
        (*nd).size = 1 + self.zsize((*nd).left) + self.zsize((*nd).right);
    }

    /// Orders node `t` relative to the key (score, member).
    unsafe fn zcmp(&self, t: u32, score: f64, member: &[u8]) -> Ordering {
        (*self.node(t))
            .score
            .partial_cmp(&score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.member_of(t).cmp(member))
    }

    /// Splits `t` into (nodes before the key, the rest). With `inclusive`, a node
    /// equal to the key goes to the left part.
    unsafe fn zsplit(&self, t: u32, score: f64, member: &[u8], inclusive: bool) -> (u32, u32) {
        if t == NIL {
            return (NIL, NIL);
        }
        let nd = self.node(t);
        let c = self.zcmp(t, score, member);
        if c == Ordering::Less || (inclusive && c == Ordering::Equal) {
            let (l, r) = self.zsplit((*nd).right, score, member, inclusive);
            (*nd).right = l;
            self.zfix(t);
            (t, r)
        } else {
            let (l, r) = self.zsplit((*nd).left, score, member, inclusive);
            (*nd).left = r;
            self.zfix(t);
            (l, t)
        }
    }

    /// Joins two treaps where every key of `a` sorts before every key of `b`.
    unsafe fn zmerge(&self, a: u32, b: u32) -> u32 {
        if a == NIL {
            return b;
        }
        if b == NIL {
            return a;
        }
        if (*self.node(a)).prio > (*self.node(b)).prio {
            let r = self.zmerge((*self.node(a)).right, b);
            (*self.node(a)).right = r;
            self.zfix(a);
            a
        } else {
            let l = self.zmerge(a, (*self.node(b)).left);
            (*self.node(b)).left = l;
            self.zfix(b);
            b
        }
    }

    /// Inserts the detached single node `n`.
    unsafe fn ztree_insert(&self, root: u32, n: u32) -> u32 {
        let (score, member) = ((*self.node(n)).score, self.member_of(n));
        let (l, r) = self.zsplit(root, score, member, false);
        self.zmerge(self.zmerge(l, n), r)
    }

    /// Detaches the node equal to (score, member); returns (new root, node or NIL).
    unsafe fn ztree_remove(&self, root: u32, score: f64, member: &[u8]) -> (u32, u32) {
        let (l, r) = self.zsplit(root, score, member, false);
        let (mid, r) = self.zsplit(r, score, member, true);
        (self.zmerge(l, r), mid)
    }

    /// 0-based ascending rank of node `n`, which must be in the tree at `root`.
    pub unsafe fn zrank_of(&self, root: u32, n: u32) -> u32 {
        let (score, member) = ((*self.node(n)).score, self.member_of(n));
        let mut t = root;
        let mut rank = 0;
        while t != NIL {
            let nd = self.node(t);
            match self.zcmp(t, score, member) {
                Ordering::Greater => t = (*nd).left,
                Ordering::Less => {
                    rank += self.zsize((*nd).left) + 1;
                    t = (*nd).right;
                }
                Ordering::Equal => return rank + self.zsize((*nd).left),
            }
        }
        rank
    }

    /// The node with 0-based ascending rank `k`, or NIL.
    pub unsafe fn zselect(&self, root: u32, mut k: u32) -> u32 {
        let mut t = root;
        while t != NIL {
            let nd = self.node(t);
            let ls = self.zsize((*nd).left);
            if k < ls {
                t = (*nd).left;
            } else if k == ls {
                return t;
            } else {
                k -= ls + 1;
                t = (*nd).right;
            }
        }
        NIL
    }

    // ---- sorted-set operations (sorted-set lock held) ----

    pub unsafe fn zset_find(&self, zv: &ZsetVal, member: &[u8]) -> Option<u32> {
        self.zidx_find(zv.zid, member_hash(zv.zid, member), member).map(|(_, n)| n)
    }

    /// Adds `member` with score `f(None)`, or changes its score to `f(Some(old))`.
    /// Returns (whether it was added, the new score). Needs the exclusive lock.
    pub unsafe fn zset_upsert(
        &self,
        zv: &mut ZsetVal,
        member: &[u8],
        f: impl FnOnce(Option<f64>) -> f64,
    ) -> Result<(bool, f64), KvError> {
        let hash = member_hash(zv.zid, member);
        match self.zidx_find(zv.zid, hash, member) {
            Some((_, n)) => {
                let old = (*self.node(n)).score;
                let new = f(Some(old));
                if new.is_nan() {
                    return Err(KvError::NotANumber);
                }
                if new != old {
                    let (root, detached) = self.ztree_remove(zv.root, old, member);
                    debug_assert_eq!(detached, n);
                    let nd = self.node(n);
                    (*nd).score = new;
                    (*nd).left = NIL;
                    (*nd).right = NIL;
                    (*nd).size = 1;
                    zv.root = self.ztree_insert(root, n);
                }
                Ok((false, new))
            }
            None => {
                let new = f(None);
                if new.is_nan() {
                    return Err(KvError::NotANumber);
                }
                let n = self.znode_alloc(zv.zid, hash, member, new).ok_or(KvError::PoolFull)?;
                self.zidx_insert(hash, n);
                zv.root = self.ztree_insert(zv.root, n);
                zv.count += 1;
                Ok((true, new))
            }
        }
    }

    /// Removes `member`; returns false if it was not there. Needs the exclusive lock.
    pub unsafe fn zset_remove(&self, zv: &mut ZsetVal, member: &[u8]) -> bool {
        let hash = member_hash(zv.zid, member);
        let Some((pos, n)) = self.zidx_find(zv.zid, hash, member) else {
            return false;
        };
        let (root, _) = self.ztree_remove(zv.root, (*self.node(n)).score, member);
        zv.root = root;
        zv.count -= 1;
        self.zidx_remove_at(pos);
        self.znode_free(n);
        true
    }

    /// Frees every node of the sorted set stored in slot `s` and leaves it empty.
    /// Needs the exclusive sorted-set lock.
    pub(crate) unsafe fn zset_free_locked(&self, s: *mut Slot) {
        let zv: ZsetVal = self.read_val_as(s);
        let mut stack = Vec::new();
        if zv.root != NIL {
            stack.push(zv.root);
        }
        while let Some(n) = stack.pop() {
            let nd = self.node(n);
            if (*nd).left != NIL {
                stack.push((*nd).left);
            }
            if (*nd).right != NIL {
                stack.push((*nd).right);
            }
            if let Some((pos, _)) = self.zidx_find((*nd).zid, (*nd).hash, self.member_of(n)) {
                self.zidx_remove_at(pos);
            }
            self.znode_free(n);
        }
        self.write_val_as(s, ZsetVal::new(zv.zid));
    }

    // ---- consistency checks for kv.verify() (sorted-set lock held) ----

    /// Checks order, subtree sizes, heap priorities and index reachability.
    pub unsafe fn zset_check(&self, zv: &ZsetVal) -> Result<(), String> {
        let mut stack = Vec::new();
        let mut t = zv.root;
        let mut prev = NIL;
        let mut visited = 0u32;
        while t != NIL || !stack.is_empty() {
            while t != NIL {
                stack.push(t);
                t = (*self.node(t)).left;
            }
            let n = stack.pop().unwrap();
            let nd = self.node(n);
            visited += 1;
            if visited > zv.count {
                return Err(format!("tree has more nodes than its count {}", zv.count));
            }
            if (*nd).in_use != 1 || (*nd).zid != zv.zid {
                return Err(format!("node {n} does not belong to this sorted set"));
            }
            if (*nd).size != 1 + self.zsize((*nd).left) + self.zsize((*nd).right) {
                return Err(format!("node {n} has a wrong subtree size"));
            }
            for child in [(*nd).left, (*nd).right] {
                if child != NIL && (*self.node(child)).prio > (*nd).prio {
                    return Err(format!("node {n} violates the heap order"));
                }
            }
            if prev != NIL && self.zcmp(prev, (*nd).score, self.member_of(n)) != Ordering::Less {
                return Err(format!("node {n} is out of order"));
            }
            if (*nd).hash != member_hash(zv.zid, self.member_of(n))
                || self.zidx_find(zv.zid, (*nd).hash, self.member_of(n)).map(|(_, m)| m) != Some(n)
            {
                return Err(format!("node {n} is not reachable through the member index"));
            }
            prev = n;
            t = (*nd).right;
        }
        if visited != zv.count || self.zsize(zv.root) != zv.count {
            return Err(format!("count {} but {} nodes in the tree", zv.count, visited));
        }
        Ok(())
    }

    /// Checks that free and used nodes add up to the pool size; returns used.
    pub unsafe fn zset_pool_check(&self) -> Result<u32, String> {
        let mut free = 0u32;
        let mut n = (*self.zhdr).free_head;
        while n != NIL {
            free += 1;
            if free > self.g.z_nodes || (*self.node(n)).in_use != 0 {
                return Err("free list is corrupt".into());
            }
            n = (*self.node(n)).left;
        }
        let used = (*self.zhdr).used;
        if free + used != self.g.z_nodes {
            return Err(format!("{free} free + {used} used != {} nodes", self.g.z_nodes));
        }
        Ok(used)
    }
}
