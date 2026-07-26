//! Cerveau F-2: durable idempotency ledger for side-effectful tool calls.
//!
//! ADR-001 §0.2 found that the control plane resumes a crashed multi-day task
//! at *conversation-turn* granularity, not with a Restate-style per-step
//! journal. That is acceptable for waiting/paused tasks, but leaves a narrow
//! window: if a turn crashes mid-execution and is later replayed, a
//! side-effectful tool it already ran (send an email, create a calendar
//! event, record an invoice) could fire twice — worse than a clean failure.
//!
//! This ledger closes that window. Before a side-effectful tool executes, the
//! caller derives a stable idempotency key and calls [`ToolIdemLedger::claim`]:
//!
//! - first sight of the key → `Claimed`; execute the tool, then record the
//!   outcome with [`ToolIdemLedger::complete`];
//! - a key already `complete` → `AlreadyDone(prior_output)`; the tool is
//!   **not** re-executed and the recorded output is reused;
//! - a key claimed but never completed (the crash case) → `InFlight`; the
//!   caller decides its retry policy (typically: proceed once, since the prior
//!   attempt did not durably record success).
//!
//! Modelled on [`super::task_store_sqlite::SqliteTaskStore`]: one SQLite table
//! behind a `parking_lot::Mutex`, WAL pragmas, no `.await` across the lock.
//! Stored in the same `control_plane.db` so it shares the task registry's
//! durability and single-writer-per-`data_dir` invariant.
//!
//! Wiring into the tool loop (which tools count as side-effectful, key
//! derivation from tenant + task + turn + tool + args) is applied separately;
//! see ADR-003. This module is the standalone, tested primitive.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Result of claiming an idempotency key before executing a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// First claim of this key — the caller should execute the tool and then
    /// call [`ToolIdemLedger::complete`].
    Claimed,
    /// A prior execution already completed for this key — reuse this output,
    /// do NOT execute the tool again.
    AlreadyDone(String),
    /// The key was claimed by an earlier attempt that never completed (a crash
    /// between claim and completion). No durable success was recorded; the
    /// caller applies its own retry policy.
    InFlight,
}

/// Derive a stable idempotency key for one side-effectful tool call.
///
/// The key binds every dimension that makes a call "the same call" on replay:
/// the owning identity (tenant/principal, empty for vanilla), the task, the
/// turn, the tool name, and a hash of the exact arguments. Two genuinely
/// distinct calls never collide; the same call on a replayed turn always
/// maps to the same key.
pub fn derive_key(principal: &str, task_id: &str, turn_id: &str, tool: &str, args: &str) -> String {
    let mut h = Sha256::new();
    for part in [principal, task_id, turn_id, tool, args] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("ti_{:x}", h.finalize())
}

/// SQLite-backed idempotency ledger for tool executions.
pub struct ToolIdemLedger {
    conn: Mutex<Connection>,
}

impl ToolIdemLedger {
    /// Open (creating if absent) the ledger in `<data_dir>/control_plane.db`.
    pub fn new(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("control_plane.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open control-plane DB: {}", db_path.display()))?;
        Self::init(conn)
    }

    /// In-memory ledger for unit tests.
    pub fn new_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("open in-memory idem ledger")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS tool_idempotency (
                 key         TEXT PRIMARY KEY,
                 status      TEXT NOT NULL,   -- 'claimed' | 'complete'
                 output      TEXT,
                 claimed_at  TEXT NOT NULL,
                 completed_at TEXT
             );",
        )
        .context("init tool_idempotency schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Atomically claim `key` for execution.
    ///
    /// Uses `INSERT ... ON CONFLICT DO NOTHING` so concurrent claimers race
    /// safely: exactly one sees the insert (→ `Claimed`), the rest read the
    /// existing row (`complete` → `AlreadyDone`, else `InFlight`).
    pub fn claim(&self, key: &str) -> Result<Claim> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let inserted = conn.execute(
            "INSERT INTO tool_idempotency (key, status, claimed_at)
             VALUES (?1, 'claimed', ?2)
             ON CONFLICT(key) DO NOTHING",
            params![key, now],
        )?;
        if inserted == 1 {
            return Ok(Claim::Claimed);
        }
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT status, output FROM tool_idempotency WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((status, output)) if status == "complete" => {
                Ok(Claim::AlreadyDone(output.unwrap_or_default()))
            }
            Some(_) => Ok(Claim::InFlight),
            // Row vanished between the insert race and the select (another
            // writer could not delete it — there is no delete path); treat as
            // a fresh claim rather than inventing state.
            None => Ok(Claim::Claimed),
        }
    }

    /// Record the successful outcome for a previously [`claim`](Self::claim)ed
    /// key. Idempotent: completing an already-complete key leaves the first
    /// recorded output in place.
    pub fn complete(&self, key: &str, output: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE tool_idempotency
                SET status = 'complete', output = ?2, completed_at = ?3
              WHERE key = ?1 AND status <> 'complete'",
            params![key, output, now],
        )?;
        Ok(())
    }

    /// Release a claimed-but-not-completed key so a caller that decided NOT to
    /// proceed (e.g. the tool errored before any side effect) doesn't leave a
    /// permanent `InFlight` tombstone blocking a legitimate retry.
    pub fn release(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM tool_idempotency WHERE key = ?1 AND status = 'claimed'",
            params![key],
        )?;
        Ok(())
    }
}

/// Process-wide singleton, lazily opened on first use and reused for the
/// life of the daemon — one `<data_dir>/control_plane.db` per process (the
/// same single-writer-per-`data_dir` invariant `ControlPlaneHandle` relies
/// on), never reopened per turn. `SqliteTaskStore`'s WAL connection would
/// tolerate a second open, but there's no reason to pay the open+schema
/// cost on every tool call when one instance already amortizes it.
static SHARED: OnceLock<Arc<ToolIdemLedger>> = OnceLock::new();

impl ToolIdemLedger {
    /// The shared ledger instance for this process, opening it at
    /// `<data_dir>/control_plane.db` on first call. Every caller after the
    /// first must pass the *same* `data_dir` (true today: one daemon per
    /// `data_dir`) — a later call with a different path silently keeps
    /// serving the first-opened instance rather than erroring, matching
    /// this being a lazy cache, not a per-call open.
    pub fn shared(data_dir: &Path) -> Result<Arc<ToolIdemLedger>> {
        if let Some(existing) = SHARED.get() {
            return Ok(Arc::clone(existing));
        }
        let opened = Arc::new(Self::new(data_dir)?);
        Ok(Arc::clone(SHARED.get_or_init(|| opened)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_key_is_stable_and_collision_resistant() {
        let a = derive_key("t1", "task", "turn", "send_email", "{\"to\":\"x\"}");
        let a2 = derive_key("t1", "task", "turn", "send_email", "{\"to\":\"x\"}");
        assert_eq!(a, a2, "same call → same key");
        // Field-boundary safety: moving a byte across a boundary changes the key.
        let b = derive_key("t1x", "", "turn", "send_email", "{\"to\":\"x\"}");
        assert_ne!(a, b);
        let c = derive_key("t1", "task", "turn", "send_email", "{\"to\":\"y\"}");
        assert_ne!(a, c, "different args → different key");
    }

    #[test]
    fn first_claim_then_complete_then_replay_reuses_output() {
        let l = ToolIdemLedger::new_in_memory().unwrap();
        let k = derive_key("t1", "task", "turn", "record_invoice", "{}");
        assert_eq!(l.claim(&k).unwrap(), Claim::Claimed);
        l.complete(&k, "invoice#4411 saved").unwrap();
        // The replayed turn must NOT re-execute — it gets the prior output.
        assert_eq!(
            l.claim(&k).unwrap(),
            Claim::AlreadyDone("invoice#4411 saved".into())
        );
    }

    #[test]
    fn claimed_but_not_completed_reports_in_flight() {
        let l = ToolIdemLedger::new_in_memory().unwrap();
        let k = derive_key("t1", "task", "turn", "send_email", "{}");
        assert_eq!(l.claim(&k).unwrap(), Claim::Claimed);
        // Simulate crash before complete: a second claim sees InFlight.
        assert_eq!(l.claim(&k).unwrap(), Claim::InFlight);
    }

    #[test]
    fn release_allows_a_fresh_claim() {
        let l = ToolIdemLedger::new_in_memory().unwrap();
        let k = derive_key("t1", "task", "turn", "send_email", "{}");
        assert_eq!(l.claim(&k).unwrap(), Claim::Claimed);
        l.release(&k).unwrap();
        assert_eq!(l.claim(&k).unwrap(), Claim::Claimed);
    }

    #[test]
    fn complete_is_idempotent_keeps_first_output() {
        let l = ToolIdemLedger::new_in_memory().unwrap();
        let k = derive_key("t1", "task", "turn", "record_invoice", "{}");
        l.claim(&k).unwrap();
        l.complete(&k, "first").unwrap();
        l.complete(&k, "second").unwrap();
        assert_eq!(l.claim(&k).unwrap(), Claim::AlreadyDone("first".into()));
    }

    #[test]
    fn distinct_keys_are_independent() {
        let l = ToolIdemLedger::new_in_memory().unwrap();
        let k1 = derive_key("t1", "task", "turn", "send_email", "{\"to\":\"a\"}");
        let k2 = derive_key("t2", "task", "turn", "send_email", "{\"to\":\"a\"}");
        l.claim(&k1).unwrap();
        l.complete(&k1, "sent to a for t1").unwrap();
        // Different tenant, same shape → independent, still claimable.
        assert_eq!(l.claim(&k2).unwrap(), Claim::Claimed);
    }
}
