//! Cerveau (enterprise-hardening round 1): durable record for tool calls
//! that resolved `ApprovalRequirement::Pending` — an `Irreversible`-tier
//! tool on a non-interactive channel, where there is no operator present to
//! prompt and no live back-channel to route the ask to (Composio/Stripe,
//! OfficeCLI — see the `approval` module's `Pending` variant for the full
//! rationale).
//!
//! Deliberately NOT a resume mechanism: a pending row does not keep the
//! original conversational turn open, and resolving it does not re-inject a
//! result into that turn (that needs Cerveau's still-unbuilt F-1
//! auto-resume driver, ADR-003). Resolving a row instead executes the tool
//! directly, out-of-band, from the resolve handler — the human sees
//! "approved, action executed," not the original chat thread continuing.
//! This is a documented, intentional simplification: the row shape here
//! (principal/tool/arguments/risk_tier) is exactly what a future F-1-driven
//! resume would also need, so this is a stepping stone, not a dead end.
//!
//! Modelled on [`super::tool_idem::ToolIdemLedger`]: one SQLite table in the
//! same `control_plane.db`, behind a `parking_lot::Mutex`, WAL pragmas.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

/// One durable pending-approval record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: String,
    /// The tenant/platform-user id, empty string for a vanilla (non-tenant)
    /// turn — never the flattened `tenant_id` (which folds in agent type),
    /// matching this fork's existing `platform_user_id` convention.
    pub principal: String,
    pub tool_name: String,
    /// Raw JSON arguments, exactly as the model called the tool.
    pub arguments: String,
    pub risk_tier: String,
    pub requested_at: String,
    /// `"pending"` | `"approved"` | `"denied"`.
    pub status: String,
    pub resolved_at: Option<String>,
    /// Free-text identity of whoever resolved it (e.g. an operator email or
    /// `"api"` for an unauthenticated dev-mode call) — an audit trail
    /// field, not an authorization check.
    pub resolved_by: Option<String>,
}

/// SQLite-backed store for pending-approval records.
pub struct PendingApprovalsStore {
    conn: Mutex<Connection>,
}

impl PendingApprovalsStore {
    /// Open (creating if absent) the store in `<data_dir>/control_plane.db`
    /// — the same file [`super::tool_idem::ToolIdemLedger`] uses; a second
    /// connection to the same WAL-mode file is safe and keeps the two
    /// concerns in separate tables/types rather than one shared struct.
    pub fn new(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("control_plane.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open control-plane DB: {}", db_path.display()))?;
        Self::init(conn)
    }

    /// In-memory store for unit tests.
    pub fn new_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("open in-memory pending-approvals store")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS pending_approvals (
                 id           TEXT PRIMARY KEY,
                 principal    TEXT NOT NULL,
                 tool_name    TEXT NOT NULL,
                 arguments    TEXT NOT NULL,
                 risk_tier    TEXT NOT NULL,
                 requested_at TEXT NOT NULL,
                 status       TEXT NOT NULL,   -- 'pending' | 'approved' | 'denied'
                 resolved_at  TEXT,
                 resolved_by  TEXT
             );",
        )
        .context("init pending_approvals schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a new pending row and return its id.
    pub fn insert(
        &self,
        principal: &str,
        tool_name: &str,
        arguments: &str,
        risk_tier: &str,
    ) -> Result<String> {
        let id = format!("pa_{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO pending_approvals
                 (id, principal, tool_name, arguments, risk_tier, requested_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
            params![id, principal, tool_name, arguments, risk_tier, now],
        )?;
        Ok(id)
    }

    /// Fetch one row by id, regardless of status.
    pub fn get(&self, id: &str) -> Result<Option<PendingApproval>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                    status, resolved_at, resolved_by
               FROM pending_approvals WHERE id = ?1",
            params![id],
            Self::row_to_pending_approval,
        )
        .optional()
        .context("query pending_approvals by id")
    }

    /// List rows, optionally filtered to one status (`"pending"` etc.).
    /// Newest first.
    pub fn list(&self, status: Option<&str>) -> Result<Vec<PendingApproval>> {
        let conn = self.conn.lock();
        let mut stmt = if status.is_some() {
            conn.prepare(
                "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                        status, resolved_at, resolved_by
                   FROM pending_approvals WHERE status = ?1 ORDER BY requested_at DESC",
            )?
        } else {
            conn.prepare(
                "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                        status, resolved_at, resolved_by
                   FROM pending_approvals ORDER BY requested_at DESC",
            )?
        };
        let rows = if let Some(s) = status {
            stmt.query_map(params![s], Self::row_to_pending_approval)?
        } else {
            stmt.query_map(params![], Self::row_to_pending_approval)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("list pending_approvals")
    }

    /// Transition a `pending` row to `approved`/`denied`. Returns `true` if
    /// a row was actually transitioned (idempotent: resolving an
    /// already-resolved id is a no-op, not an error — the caller decides
    /// whether that's worth surfacing).
    pub fn resolve(&self, id: &str, status: &str, resolved_by: &str) -> Result<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let updated = conn.execute(
            "UPDATE pending_approvals
                SET status = ?2, resolved_at = ?3, resolved_by = ?4
              WHERE id = ?1 AND status = 'pending'",
            params![id, status, now, resolved_by],
        )?;
        Ok(updated == 1)
    }

    fn row_to_pending_approval(r: &rusqlite::Row) -> rusqlite::Result<PendingApproval> {
        Ok(PendingApproval {
            id: r.get(0)?,
            principal: r.get(1)?,
            tool_name: r.get(2)?,
            arguments: r.get(3)?,
            risk_tier: r.get(4)?,
            requested_at: r.get(5)?,
            status: r.get(6)?,
            resolved_at: r.get(7)?,
            resolved_by: r.get(8)?,
        })
    }
}

/// Process-wide singleton, mirroring
/// [`super::tool_idem::ToolIdemLedger::shared`] — lazily opened on first
/// use, reused for the life of the daemon, one `<data_dir>/control_plane.db`
/// per process.
static SHARED: OnceLock<Arc<PendingApprovalsStore>> = OnceLock::new();

impl PendingApprovalsStore {
    pub fn shared(data_dir: &Path) -> Result<Arc<PendingApprovalsStore>> {
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
    fn insert_then_get_round_trips() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store
            .insert("tenant-1", "finalize_invoice", "{}", "irreversible")
            .unwrap();
        let row = store.get(&id).unwrap().expect("row must exist");
        assert_eq!(row.principal, "tenant-1");
        assert_eq!(row.tool_name, "finalize_invoice");
        assert_eq!(row.status, "pending");
        assert!(row.resolved_at.is_none());
    }

    #[test]
    fn list_filters_by_status() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let a = store.insert("t1", "tool_a", "{}", "irreversible").unwrap();
        let _b = store.insert("t1", "tool_b", "{}", "irreversible").unwrap();
        store.resolve(&a, "approved", "ops@example.com").unwrap();

        let pending = store.list(Some("pending")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_name, "tool_b");

        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn resolve_is_idempotent_second_call_is_noop() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store.insert("t1", "tool_a", "{}", "irreversible").unwrap();
        assert!(store.resolve(&id, "approved", "ops").unwrap());
        // Second resolve of the same (now non-pending) row is a no-op, not
        // an overwrite — the first resolution wins.
        assert!(!store.resolve(&id, "denied", "someone-else").unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert_eq!(row.resolved_by.as_deref(), Some("ops"));
    }

    #[test]
    fn resolve_unknown_id_returns_false_not_error() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        assert!(!store.resolve("pa_does-not-exist", "approved", "ops").unwrap());
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        assert!(store.get("pa_does-not-exist").unwrap().is_none());
    }
}
