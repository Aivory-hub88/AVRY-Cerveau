//! Redis-backed sliding-window rate limiter (ADR-005).
//!
//! Same sliding-window semantics as [`crate::SlidingWindowRateLimiter`], but
//! the counters live in Redis instead of a per-process `Mutex<HashMap<..>>`
//! so N instances behind a load balancer see one consistent view of a given
//! key's recent request timestamps. Existing in-process limiter is untouched
//! and remains the default — this is purely an opt-in alternate backend.

use redis::aio::ConnectionManager;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Atomic prune-check-insert-expire in one round trip. `KEYS[1]` is the
/// per-key sorted set (score = insertion time in ms); `KEYS[2]` is a
/// companion counter used only to mint a unique sorted-set member per
/// admitted request.
///
/// The member MUST be minted via Redis's own atomic `INCR` (`KEYS[2]`), not
/// client-side — this was tried first (a per-process `AtomicU64` appended to
/// the timestamp) and a live test caught it undercounting: two *separate*
/// processes each start their own counter at 0, so two concurrent callers on
/// different instances can independently compute the identical
/// `"{now_ms}-0"` member, `ZADD` silently no-ops on the duplicate, and the
/// shared cardinality this whole limiter exists to enforce quietly drifts
/// low. `INCR` is atomic across every caller regardless of which process or
/// host it runs on, which a client-side counter can never be.
const SLIDING_WINDOW_SCRIPT: &str = r"
local key = KEYS[1]
local seq_key = KEYS[2]
local now_ms = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
redis.call('ZREMRANGEBYSCORE', key, '-inf', now_ms - window_ms)
if redis.call('ZCARD', key) >= limit then
    return 0
end
local seq = redis.call('INCR', seq_key)
redis.call('PEXPIRE', seq_key, window_ms)
redis.call('ZADD', key, now_ms, now_ms .. '-' .. seq)
redis.call('PEXPIRE', key, window_ms)
return 1
";

/// A single named sliding-window limiter backed by Redis (e.g. the "pair",
/// "webhook", or "tenant" limiter each get their own instance, sharing one
/// underlying [`ConnectionManager`] via cheap `Clone`).
#[derive(Debug, Clone)]
pub struct RedisRateLimiter {
    conn: ConnectionManager,
    script: redis::Script,
    key_prefix: String,
    limit_per_window: u32,
    window: Duration,
}

impl RedisRateLimiter {
    /// Connect to `redis_url` and build a limiter enforcing `limit_per_window`
    /// requests per `window`, with keys namespaced under `key_prefix`.
    pub async fn connect(
        redis_url: &str,
        key_prefix: String,
        limit_per_window: u32,
        window: Duration,
    ) -> redis::RedisResult<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = client.get_connection_manager().await?;
        Ok(Self {
            conn,
            script: redis::Script::new(SLIDING_WINDOW_SCRIPT),
            key_prefix,
            limit_per_window,
            window,
        })
    }

    /// Build a second named limiter (different limit/window) reusing an
    /// already-connected [`ConnectionManager`] — avoids a second TCP/AUTH
    /// round trip when constructing the pair/webhook/tenant limiters
    /// together at startup.
    pub fn with_shared_connection(
        &self,
        limit_per_window: u32,
        window: Duration,
    ) -> Self {
        Self {
            conn: self.conn.clone(),
            script: redis::Script::new(SLIDING_WINDOW_SCRIPT),
            key_prefix: self.key_prefix.clone(),
            limit_per_window,
            window,
        }
    }

    /// Returns `true` if `key` is under its limit for the current window
    /// (and records this call toward it), `false` if it's over.
    ///
    /// **Fails open**: a Redis error (timeout, connection drop) is logged
    /// and treated as "allow" rather than blocking the request path on a
    /// dependency hiccup — the same posture already chosen for the F-2
    /// idempotency ledger.
    pub async fn allow(&self, key: &str) -> bool {
        if self.limit_per_window == 0 {
            return true;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let window_ms = self.window.as_millis() as i64;
        let full_key = format!("{}{}", self.key_prefix, key);
        let seq_key = format!("{full_key}:seq");

        let mut conn = self.conn.clone();
        let result: redis::RedisResult<i64> = self
            .script
            .key(full_key)
            .key(seq_key)
            .arg(now_ms)
            .arg(window_ms)
            .arg(self.limit_per_window)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(1) => true,
            Ok(_) => false,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                    "redis rate limiter error — failing open for this request"
                );
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only runs when `CERVEAU_TEST_REDIS_URL` is set (CI provides a redis
    /// service container). Absent that env var the test is a no-op so the
    /// suite stays green without Redis, mirroring `CERVEAU_TEST_PG_URL`'s
    /// established pattern for the Postgres lifecycle tests.
    fn test_redis_url() -> Option<String> {
        std::env::var("CERVEAU_TEST_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    #[tokio::test]
    async fn redis_limiter_blocks_after_limit_and_recovers_after_window() {
        let Some(url) = test_redis_url() else {
            eprintln!("skipping: CERVEAU_TEST_REDIS_URL not set");
            return;
        };
        let prefix = format!("test:{}:", uuid_like());
        let limiter = RedisRateLimiter::connect(&url, prefix, 2, Duration::from_millis(300))
            .await
            .expect("connect");

        assert!(limiter.allow("k").await);
        assert!(limiter.allow("k").await);
        assert!(!limiter.allow("k").await, "third call should be blocked");

        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(
            limiter.allow("k").await,
            "window elapsed — should be allowed again"
        );
    }

    /// The property that is impossible to prove with the in-process limiter
    /// once there's more than one instance: two independently-connected
    /// `RedisRateLimiter`s (standing in for two Cerveau instances) sharing
    /// the same key must enforce ONE aggregate limit, not one each.
    #[tokio::test]
    async fn redis_limiter_shares_state_across_independent_connections() {
        let Some(url) = test_redis_url() else {
            eprintln!("skipping: CERVEAU_TEST_REDIS_URL not set");
            return;
        };
        let prefix = format!("test:{}:", uuid_like());
        let instance_a = RedisRateLimiter::connect(&url, prefix.clone(), 3, Duration::from_secs(60))
            .await
            .expect("connect a");
        let instance_b = RedisRateLimiter::connect(&url, prefix, 3, Duration::from_secs(60))
            .await
            .expect("connect b");

        assert!(instance_a.allow("shared-tenant").await);
        assert!(instance_b.allow("shared-tenant").await);
        assert!(instance_a.allow("shared-tenant").await);
        // Aggregate of 3 across both "instances" now reached — a 4th call
        // on EITHER connection must be blocked, proving the state is
        // genuinely shared, not per-connection.
        assert!(!instance_b.allow("shared-tenant").await);
        assert!(!instance_a.allow("shared-tenant").await);
    }

    #[tokio::test]
    async fn redis_limiter_different_keys_are_independent() {
        let Some(url) = test_redis_url() else {
            eprintln!("skipping: CERVEAU_TEST_REDIS_URL not set");
            return;
        };
        let prefix = format!("test:{}:", uuid_like());
        let limiter = RedisRateLimiter::connect(&url, prefix, 1, Duration::from_secs(60))
            .await
            .expect("connect");

        assert!(limiter.allow("tenant-a").await);
        assert!(!limiter.allow("tenant-a").await);
        assert!(
            limiter.allow("tenant-b").await,
            "a different key must not be affected by tenant-a's cap"
        );
    }

    #[tokio::test]
    async fn redis_limiter_zero_limit_always_allows() {
        let Some(url) = test_redis_url() else {
            eprintln!("skipping: CERVEAU_TEST_REDIS_URL not set");
            return;
        };
        let prefix = format!("test:{}:", uuid_like());
        let limiter = RedisRateLimiter::connect(&url, prefix, 0, Duration::from_secs(60))
            .await
            .expect("connect");
        for _ in 0..5 {
            assert!(limiter.allow("k").await);
        }
    }

    /// Cheap process-local uniqueness for test key prefixes — avoids test
    /// runs colliding on a shared Redis without pulling in a `uuid` crate
    /// just for this.
    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{now}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}
