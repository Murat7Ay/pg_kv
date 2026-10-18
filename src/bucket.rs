//! Token-bucket rate limiting.
//!
//! A bucket holds up to `capacity` tokens and refills continuously at
//! `capacity / per`. Each call takes `cost` tokens if available. The bucket is
//! stored as a normal key whose TTL is set to the moment it would be full again:
//! an absent bucket means a full one, so idle buckets simply expire and the
//! sweeper reclaims them.

use crate::store::{is_expired, KvError, Kv, KIND_BUCKET};
use std::sync::atomic::Ordering::Relaxed;

#[repr(C)]
#[derive(Clone, Copy)]
struct BucketVal {
    tokens: f64,
    /// TimestampTz of the last refill.
    last: i64,
}

pub struct RateLimit {
    pub allowed: bool,
    pub remaining: i32,
    /// How long until `cost` tokens are available; 0 when allowed.
    pub retry_after_us: i64,
    /// How long until the bucket is full again.
    pub reset_after_us: i64,
}

impl Kv {
    /// Needs the shard's exclusive lock and must be called without the
    /// sorted-set lock (an expired sorted set under this key gets freed).
    pub unsafe fn take_tokens(
        &self,
        shard: u32,
        hash: u64,
        key: &[u8],
        capacity: f64,
        rate_per_us: f64,
        cost: f64,
        now: i64,
    ) -> Result<RateLimit, KvError> {
        let existing = self.find(shard, hash, key);
        let mut tokens = capacity;
        if let Some(i) = existing {
            let s = self.slot(shard, i);
            if !is_expired(s, now) {
                if (*s).kind != KIND_BUCKET {
                    return Err(KvError::WrongType);
                }
                let b: BucketVal = self.read_val_as(s);
                let elapsed = (now - b.last).max(0) as f64;
                tokens = (b.tokens + elapsed * rate_per_us).min(capacity);
            }
        }

        let allowed = tokens >= cost;
        if allowed {
            tokens -= cost;
        }
        let retry_after_us = if allowed { 0 } else { ((cost - tokens) / rate_per_us).ceil() as i64 };
        let reset_after_us = ((capacity - tokens) / rate_per_us).ceil() as i64;
        let expire_at = now.saturating_add(reset_after_us.max(1));

        let state = BucketVal { tokens, last: now };
        match existing {
            Some(i) => {
                let s = self.slot(shard, i);
                self.release_value(s);
                (*s).kind = KIND_BUCKET;
                self.write_val_as(s, state);
                self.set_expiry(shard, s, expire_at);
                (*s).last_access.store(now, Relaxed);
            }
            None => {
                self.insert_new(shard, hash, key, crate::store::as_bytes(&state), KIND_BUCKET, expire_at, now);
            }
        }
        Ok(RateLimit {
            allowed,
            remaining: tokens.floor() as i32,
            retry_after_us,
            reset_after_us,
        })
    }
}
