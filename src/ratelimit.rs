//! In-memory token-bucket rate limiter. Sharded to keep lock contention low.
//! State lives outside `Runtime`, so config hot-swaps do not reset anyone's budget.

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Result};

use crate::config::RatePolicy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateKey {
    Ip,
    Global,
    Header(String),
}

impl RateKey {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ip" => Ok(Self::Ip),
            "global" => Ok(Self::Global),
            h if h.starts_with("header:") && h.len() > 7 => {
                Ok(Self::Header(h[7..].to_ascii_lowercase()))
            }
            other => {
                bail!("unknown rate limit key `{other}` (expected ip | global | header:<name>)")
            }
        }
    }
}

/// A policy resolved at compile time.
#[derive(Debug, Clone)]
pub struct CompiledRate {
    pub policy: RatePolicy,
    pub key: RateKey,
    /// Bucket namespace: route id for per-route policies, `default` for the shared one.
    pub scope: String,
}

impl CompiledRate {
    pub fn new(policy: RatePolicy, scope: impl Into<String>) -> Result<Self> {
        let key = RateKey::parse(&policy.key)?;
        Ok(Self {
            policy,
            key,
            scope: scope.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub retry_after_secs: u64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
    /// After this much idle time the bucket is full again, i.e. indistinguishable from new.
    full_after: Duration,
}

const SHARDS: usize = 16;
const SWEEP_EVERY: u64 = 4096;

pub struct RateLimiter {
    shards: Vec<Mutex<HashMap<String, Bucket>>>,
    ops: AtomicU64,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            ops: AtomicU64::new(0),
        }
    }

    pub fn check(&self, key: &str, policy: &RatePolicy) -> Decision {
        self.check_at(key, policy, Instant::now())
    }

    pub fn check_at(&self, key: &str, policy: &RatePolicy, now: Instant) -> Decision {
        let capacity = policy.burst.unwrap_or(policy.requests).max(1) as f64;
        let rate = policy.requests as f64 / policy.per_seconds as f64; // tokens per second
        let full_after = Duration::from_secs_f64(capacity / rate);

        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        let shard = &self.shards[(h.finish() as usize) % SHARDS];
        let mut map = shard.lock().unwrap_or_else(|e| e.into_inner());

        if self.ops.fetch_add(1, Ordering::Relaxed) % SWEEP_EVERY == SWEEP_EVERY - 1 {
            map.retain(|_, b| now.saturating_duration_since(b.last) < b.full_after);
        }

        let b = map.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
            full_after,
        });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(capacity);
        b.last = now;
        b.full_after = full_after;

        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Decision {
                allowed: true,
                limit: policy.requests,
                remaining: b.tokens.floor() as u32,
                retry_after_secs: 0,
            }
        } else {
            let wait = ((1.0 - b.tokens) / rate).ceil().max(1.0) as u64;
            Decision {
                allowed: false,
                limit: policy.requests,
                remaining: 0,
                retry_after_secs: wait,
            }
        }
    }

    /// True if `key` currently has no budget. Does not consume anything.
    pub fn is_blocked(&self, key: &str, policy: &RatePolicy) -> bool {
        let capacity = policy.burst.unwrap_or(policy.requests).max(1) as f64;
        let rate = policy.requests as f64 / policy.per_seconds as f64;
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        let map = self.shards[(h.finish() as usize) % SHARDS]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            None => false,
            Some(b) => (b.tokens + b.last.elapsed().as_secs_f64() * rate).min(capacity) < 1.0,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(requests: u32, per: u64, burst: Option<u32>) -> RatePolicy {
        RatePolicy {
            requests,
            per_seconds: per,
            burst,
            key: "ip".into(),
        }
    }

    #[test]
    fn burst_then_block_then_refill() {
        let rl = RateLimiter::new();
        let p = policy(2, 1, None); // 2 req/s, bucket 2
        let t0 = Instant::now();
        assert!(rl.check_at("k", &p, t0).allowed);
        let d = rl.check_at("k", &p, t0);
        assert!(d.allowed && d.remaining == 0);
        let blocked = rl.check_at("k", &p, t0);
        assert!(!blocked.allowed);
        assert_eq!(blocked.retry_after_secs, 1);
        // half a second later one token is back
        assert!(
            rl.check_at("k", &p, t0 + Duration::from_millis(500))
                .allowed
        );
        assert!(
            !rl.check_at("k", &p, t0 + Duration::from_millis(500))
                .allowed
        );
    }

    #[test]
    fn keys_are_independent_and_burst_is_honored() {
        let rl = RateLimiter::new();
        let p = policy(1, 60, Some(3));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(rl.check_at("a", &p, t0).allowed);
        }
        assert!(!rl.check_at("a", &p, t0).allowed);
        assert!(rl.check_at("b", &p, t0).allowed);
    }

    #[test]
    fn idle_buckets_are_swept() {
        let rl = RateLimiter::new();
        let p = policy(10, 1, None);
        let t0 = Instant::now();
        for i in 0..100 {
            rl.check_at(&format!("k{i}"), &p, t0);
        }
        assert_eq!(rl.len(), 100);
        let later = t0 + Duration::from_secs(60);
        for i in 0..SWEEP_EVERY {
            rl.check_at("hot", &p, later + Duration::from_millis(i));
        }
        assert!(
            rl.len() < 100,
            "idle buckets should have been dropped, got {}",
            rl.len()
        );
    }

    #[test]
    fn key_parsing() {
        assert_eq!(RateKey::parse("ip").unwrap(), RateKey::Ip);
        assert_eq!(
            RateKey::parse("header:X-Api-Key").unwrap(),
            RateKey::Header("x-api-key".into())
        );
        assert!(RateKey::parse("header:").is_err());
        assert!(RateKey::parse("user").is_err());
    }
}
