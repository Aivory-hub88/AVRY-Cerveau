//! Per-instance admission queue for the `/webhook` LLM-calling path (ADR-005).
//!
//! Purely local to this instance — deliberately not Redis-backed, unlike the
//! rate limiter. The problem this solves (a CPU-bound instance about to take
//! on more concurrent work than it can serve) is per-instance by nature: the
//! fleet-wide ceiling belongs at the load balancer (Traefik's `inFlightReq`,
//! see ADR-005 §3c), not duplicated here. This queue only smooths *local*
//! bursts and converts *local* sustained overload into a clean `503` instead
//! of whatever failure mode CPU starvation produces today (Phase 5 Finding
//! 4's leading hypothesis).

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Held for the duration of one admitted request; releases its slot on drop.
pub struct AdmissionPermit(#[allow(dead_code)] OwnedSemaphorePermit);

/// Returned when a request couldn't get a slot within the configured wait.
#[derive(Debug)]
pub struct AdmissionTimeout;

pub struct AdmissionQueue {
    semaphore: Arc<Semaphore>,
    timeout: Duration,
}

impl AdmissionQueue {
    /// `max_concurrent` is clamped to at least 1 — a value of 0 would wedge
    /// every request permanently, which is never the intent of "cap
    /// concurrency," so treat it the same way [`SlidingWindowRateLimiter`]
    /// treats a zero rate limit: as "not actually configured," not as
    /// "block everything."
    pub fn new(max_concurrent: u32, timeout: Duration) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent.max(1) as usize)),
            timeout,
        }
    }

    /// Waits up to the configured timeout for a free slot. A request that
    /// gets one immediately (the common case, well under capacity) pays
    /// only the cost of an uncontended semaphore acquire — no artificial
    /// delay is ever added to a request that didn't need to wait.
    pub async fn acquire(&self) -> Result<AdmissionPermit, AdmissionTimeout> {
        match tokio::time::timeout(self.timeout, self.semaphore.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(AdmissionPermit(permit)),
            // `Semaphore::close()` is never called on this instance, so this
            // is unreachable in practice; fail closed (503, same as a
            // timeout) rather than assume it's safe to let the request
            // through with no admission control at all.
            Ok(Err(_closed)) => Err(AdmissionTimeout),
            Err(_elapsed) => Err(AdmissionTimeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admits_up_to_capacity_immediately() {
        let queue = AdmissionQueue::new(2, Duration::from_millis(50));
        let a = queue.acquire().await;
        let b = queue.acquire().await;
        assert!(a.is_ok());
        assert!(b.is_ok());
    }

    #[tokio::test]
    async fn blocks_then_times_out_when_over_capacity() {
        let queue = AdmissionQueue::new(1, Duration::from_millis(50));
        let _held = queue.acquire().await.expect("first acquire succeeds");
        let start = tokio::time::Instant::now();
        let second = queue.acquire().await;
        assert!(second.is_err(), "over capacity — must time out, not admit");
        assert!(
            start.elapsed() >= Duration::from_millis(45),
            "must actually wait close to the configured timeout, not fail instantly"
        );
    }

    #[tokio::test]
    async fn a_freed_slot_unblocks_a_waiting_acquire() {
        let queue = Arc::new(AdmissionQueue::new(1, Duration::from_millis(500)));
        let held = queue.acquire().await.expect("first acquire succeeds");

        let queue2 = queue.clone();
        let waiter = tokio::spawn(async move { queue2.acquire().await.is_ok() });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held); // free the slot well before the 500ms timeout

        assert!(
            waiter.await.expect("task didn't panic"),
            "release should have unblocked the waiter within its timeout"
        );
    }

    #[tokio::test]
    async fn zero_configured_concurrency_is_treated_as_at_least_one() {
        let queue = AdmissionQueue::new(0, Duration::from_millis(50));
        assert!(
            queue.acquire().await.is_ok(),
            "max_concurrent=0 must not permanently wedge every request"
        );
    }
}
