//! Process-wide admission gate for Docker sandbox executions.
//!
//! Every Docker-backed execution (`shell` on the Docker runtime and
//! `python_execute`) is a fresh `docker run --rm`. Nothing bounded how many of
//! those ran at once, so a burst from one tenant could exhaust the host's CPU
//! and memory for every other tenant. The gate admits at most
//! `max_concurrent` runs process-wide, at most `max_concurrent_per_workspace`
//! per workspace, queues the rest in FIFO order, and refuses new work for a
//! workspace whose queue is already `max_queued_per_workspace` deep or when a
//! run waited longer than `queue_timeout`.
//!
//! The gate is per process. Running several daemons behind a load balancer
//! multiplies the effective ceiling by the number of daemons.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, timeout_at};

/// Admission limits. A `0` for either concurrency limit means "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxGateLimits {
    pub max_concurrent: usize,
    pub max_concurrent_per_workspace: usize,
    pub max_queued_per_workspace: usize,
    pub queue_timeout: Duration,
    /// Not enforced by the gate itself: the `--pids-limit` that callers
    /// building their own `docker run` (`python_execute`) should apply.
    pub pids_limit: Option<u64>,
}

impl From<&crate::schema::DockerRuntimeConfig> for SandboxGateLimits {
    fn from(config: &crate::schema::DockerRuntimeConfig) -> Self {
        Self {
            max_concurrent: config.max_concurrent,
            max_concurrent_per_workspace: config.max_concurrent_per_workspace,
            max_queued_per_workspace: config.max_queued_per_workspace,
            queue_timeout: Duration::from_secs(config.queue_timeout_secs),
            pids_limit: config.pids_limit,
        }
    }
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum SandboxGateError {
    #[error(
        "sandbox is busy: {waiting} runs are already queued for this workspace (limit {limit}); try again shortly"
    )]
    QueueFull { waiting: usize, limit: usize },
    #[error("sandbox is busy: no free slot after waiting {waited_secs:.1}s; try again shortly")]
    Timeout { waited_secs: f64 },
}

struct KeyEntry {
    sem: Arc<Semaphore>,
    /// Runs of this workspace still acquiring (not yet admitted).
    waiting: usize,
    /// Runs of this workspace that are waiting or running; the entry is
    /// dropped from the map when it reaches zero.
    users: usize,
}

pub struct SandboxGate {
    limits: SandboxGateLimits,
    global: Option<Arc<Semaphore>>,
    keys: Mutex<HashMap<String, KeyEntry>>,
}

/// Held for the lifetime of one sandboxed run. Dropping it frees the slot.
pub struct SandboxPermit {
    _global: Option<OwnedSemaphorePermit>,
    _workspace: OwnedSemaphorePermit,
    _ticket: Ticket,
}

/// Bookkeeping for one run. Dropping it, including when the acquiring future
/// is cancelled, keeps the per-workspace counters exact.
struct Ticket {
    gate: Arc<SandboxGate>,
    key: String,
    waiting: bool,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut keys = self.gate.keys.lock();
        if let Some(entry) = keys.get_mut(&self.key) {
            entry.users = entry.users.saturating_sub(1);
            if self.waiting {
                entry.waiting = entry.waiting.saturating_sub(1);
            }
            if entry.users == 0 {
                keys.remove(&self.key);
            }
        }
    }
}

impl SandboxGate {
    pub fn new(limits: SandboxGateLimits) -> Self {
        Self {
            global: (limits.max_concurrent > 0)
                .then(|| Arc::new(Semaphore::new(limits.max_concurrent))),
            limits,
            keys: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> SandboxGateLimits {
        self.limits
    }

    /// Wait for a slot for `key` (the workspace path). Both semaphores are
    /// FIFO, and the per-workspace one is taken first so a workspace that is at
    /// its own limit never sits on a global slot while it waits.
    pub async fn acquire(self: &Arc<Self>, key: &str) -> Result<SandboxPermit, SandboxGateError> {
        let started = Instant::now();
        let deadline = started + self.limits.queue_timeout;

        let workspace_sem = {
            let mut keys = self.keys.lock();
            let per_workspace = match self.limits.max_concurrent_per_workspace {
                0 => Semaphore::MAX_PERMITS,
                n => n,
            };
            let entry = keys.entry(key.to_owned()).or_insert_with(|| KeyEntry {
                sem: Arc::new(Semaphore::new(per_workspace)),
                waiting: 0,
                users: 0,
            });
            if entry.sem.available_permits() == 0
                && entry.waiting >= self.limits.max_queued_per_workspace
            {
                return Err(SandboxGateError::QueueFull {
                    waiting: entry.waiting,
                    limit: self.limits.max_queued_per_workspace,
                });
            }
            entry.waiting += 1;
            entry.users += 1;
            entry.sem.clone()
        };
        let mut ticket = Ticket {
            gate: Arc::clone(self),
            key: key.to_owned(),
            waiting: true,
        };

        let timed_out = || SandboxGateError::Timeout {
            waited_secs: started.elapsed().as_secs_f64(),
        };

        let workspace = match timeout_at(deadline, workspace_sem.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => return Err(timed_out()),
        };
        let global = match &self.global {
            Some(sem) => match timeout_at(deadline, sem.clone().acquire_owned()).await {
                Ok(Ok(permit)) => Some(permit),
                _ => return Err(timed_out()),
            },
            None => None,
        };

        // Admitted: stop counting as queued, keep counting as a user.
        if let Some(entry) = self.keys.lock().get_mut(key) {
            entry.waiting = entry.waiting.saturating_sub(1);
        }
        ticket.waiting = false;

        Ok(SandboxPermit {
            _global: global,
            _workspace: workspace,
            _ticket: ticket,
        })
    }

    /// Number of workspaces currently tracked (waiting or running).
    pub fn tracked_workspaces(&self) -> usize {
        self.keys.lock().len()
    }
}

static GLOBAL_GATE: OnceLock<Arc<SandboxGate>> = OnceLock::new();

/// Install the process-wide gate. The first caller wins; later calls (for
/// example after a config reload) return the existing gate unchanged, so a
/// limit change takes effect on the next daemon restart.
pub fn init_global(limits: SandboxGateLimits) -> Arc<SandboxGate> {
    Arc::clone(GLOBAL_GATE.get_or_init(|| Arc::new(SandboxGate::new(limits))))
}

/// The process-wide gate, if a Docker runtime has been constructed.
pub fn global() -> Option<Arc<SandboxGate>> {
    GLOBAL_GATE.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(global: usize, per_ws: usize, queued: usize, wait_ms: u64) -> SandboxGateLimits {
        SandboxGateLimits {
            max_concurrent: global,
            max_concurrent_per_workspace: per_ws,
            max_queued_per_workspace: queued,
            queue_timeout: Duration::from_millis(wait_ms),
            pids_limit: None,
        }
    }

    #[tokio::test]
    async fn global_cap_blocks_then_releases() {
        let gate = Arc::new(SandboxGate::new(limits(1, 0, 8, 60)));
        let first = gate.acquire("ws-a").await.expect("first slot");

        let blocked = gate.acquire("ws-b").await.err().expect("must time out");
        assert!(matches!(blocked, SandboxGateError::Timeout { .. }));

        drop(first);
        gate.acquire("ws-b").await.expect("slot freed after drop");
    }

    #[tokio::test]
    async fn workspace_at_its_limit_does_not_hold_a_global_slot() {
        let gate = Arc::new(SandboxGate::new(limits(2, 1, 8, 60)));
        let _a1 = gate.acquire("ws-a").await.expect("a first");

        // A second ws-a run waits on its own limit and must not consume the
        // second global slot while it does.
        let a2 = gate.acquire("ws-a").await.err().expect("a second blocked");
        assert!(matches!(a2, SandboxGateError::Timeout { .. }));
        gate.acquire("ws-b")
            .await
            .expect("other workspace unaffected");
    }

    #[tokio::test]
    async fn full_queue_is_refused_immediately() {
        let gate = Arc::new(SandboxGate::new(limits(4, 1, 1, 2_000)));
        let _running = gate.acquire("ws-a").await.expect("running");

        let waiter_gate = Arc::clone(&gate);
        let waiter = tokio::spawn(async move { waiter_gate.acquire("ws-a").await.is_ok() });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let started = Instant::now();
        let refused = gate.acquire("ws-a").await.err().expect("queue is full");
        assert_eq!(
            refused,
            SandboxGateError::QueueFull {
                waiting: 1,
                limit: 1
            }
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        waiter.abort();
    }

    #[tokio::test]
    async fn cancelled_and_finished_runs_leave_no_state_behind() {
        let gate = Arc::new(SandboxGate::new(limits(1, 1, 8, 2_000)));
        let running = gate.acquire("ws-a").await.expect("running");

        // Cancel an acquire mid-wait by dropping its future.
        let cancelled = tokio::time::timeout(Duration::from_millis(20), gate.acquire("ws-a")).await;
        assert!(cancelled.is_err());

        drop(running);
        assert_eq!(gate.tracked_workspaces(), 0);
        gate.acquire("ws-a").await.expect("slot still usable");
    }

    #[tokio::test]
    async fn zero_means_unlimited() {
        let gate = Arc::new(SandboxGate::new(limits(0, 0, 0, 50)));
        let mut held = Vec::new();
        for _ in 0..32 {
            held.push(gate.acquire("ws-a").await.expect("unlimited"));
        }
    }
}
