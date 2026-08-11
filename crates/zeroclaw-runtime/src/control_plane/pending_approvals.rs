//! Cerveau (enterprise-hardening round 1): durable record for tool calls
//! that resolved `ApprovalRequirement::Pending` — an `Irreversible`-tier
//! tool on a non-interactive channel, where there is no operator present to
//! prompt and no live back-channel to route the ask to (Composio/Stripe,
//! OfficeCLI — see the `approval` module's `Pending` variant for the full
//! rationale).
//!
//! As of patch 0028, a row also optionally carries enough context
//! (`tenant_id`/`agent_type`/`session_id`/`origin_message`, via
//! [`PendingApprovalsStore::insert_with_context`]) for a *tenant-scoped*
//! resolve path (patch 0029/0030,
//! `zeroclaw_gateway::api_tenant_approvals`) to durably resume the original
//! conversation instead of just executing the tool out-of-band — see that
//! module for the resume mechanics. The loopback-only admin resolve path
//! (`zeroclaw_gateway::api_approvals::execute_approved_tool`) is unchanged
//! by this and still only executes the tool directly; it has no channel to
//! resume a reply into.
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
    /// Cerveau (patch 0028): the tenant's flattened id
    /// (`TenantContext::tenant_id`), when this row was created from a
    /// tenant webhook turn — `None` for a loopback/CLI-originated row.
    /// The tenant-scoped resolve path (`api_tenant_approvals`) uses this as
    /// the authorization boundary: a caller may only resolve a row whose
    /// `tenant_id` matches its own authenticated tenant.
    pub tenant_id: Option<String>,
    /// Cerveau (patch 0028): the tenant's Aivory agent type
    /// (`TenantContext::agent_type`), needed to re-resolve the serving
    /// host alias and persona when synthesizing a continuation turn.
    pub agent_type: Option<String>,
    /// Cerveau (patch 0028): the original turn's session id, so a resumed
    /// continuation turn's memory recall sees the same tenant facts the
    /// original turn did.
    pub session_id: Option<String>,
    /// Cerveau (patch 0028): the verbatim user message that started the
    /// original turn — captured now because a webhook-driven turn has no
    /// saved transcript to pull it back out of later. `None` for a row
    /// inserted via the plain [`Self::insert`] (no origin context available).
    pub origin_message: Option<String>,
    /// Cerveau (patch 0032): when the continuation-turn reply for this
    /// (already-`approved`/`denied`) row was actually handed back to a
    /// caller — either synchronously, in `handle_webhook_approval_resolve`'s
    /// own HTTP response, or later via the reaper's redelivery sweep.
    /// `None` on a still-`pending` row (nothing to deliver yet) and on a
    /// resolved row whose reply generation crashed or is still in flight —
    /// that `None` state is exactly what makes a row a reaper candidate;
    /// see [`Self::list_undelivered_resolved`].
    pub delivered_at: Option<String>,
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
        // Patch 0028: additive columns for durable tenant-turn resume.
        // SQLite has no `ADD COLUMN IF NOT EXISTS` — the standard-idiom
        // migration here is one `ALTER TABLE` per column, tolerating
        // exactly the "duplicate column name" error a re-run against an
        // already-migrated (post-0028) DB produces, and propagating any
        // other error as real. Safe on both a brand-new DB (columns never
        // existed) and a pre-0028 DB (columns genuinely added for the
        // first time).
        for (column, ddl) in [
            ("tenant_id", "ALTER TABLE pending_approvals ADD COLUMN tenant_id TEXT"),
            ("agent_type", "ALTER TABLE pending_approvals ADD COLUMN agent_type TEXT"),
            ("session_id", "ALTER TABLE pending_approvals ADD COLUMN session_id TEXT"),
            (
                "origin_message",
                "ALTER TABLE pending_approvals ADD COLUMN origin_message TEXT",
            ),
            (
                "delivered_at",
                "ALTER TABLE pending_approvals ADD COLUMN delivered_at TEXT",
            ),
        ] {
            if let Err(e) = conn.execute(ddl, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e).with_context(|| format!("add pending_approvals.{column} column"));
                }
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a new pending row and return its id. No origin context is
    /// recorded — a row inserted this way can only ever be resolved
    /// out-of-band (`api_approvals::execute_approved_tool`), never
    /// durably resumed. Kept for any non-tenant (loopback/CLI) caller.
    pub fn insert(
        &self,
        principal: &str,
        tool_name: &str,
        arguments: &str,
        risk_tier: &str,
    ) -> Result<String> {
        self.insert_with_context(principal, tool_name, arguments, risk_tier, None, None, None, None)
    }

    /// Insert a new pending row carrying enough context
    /// (`tenant_id`/`agent_type`/`session_id`/`origin_message`) for a
    /// later tenant-scoped resolve call (`zeroclaw_gateway::api_tenant_approvals`,
    /// patch 0029) to synthesize a coherent continuation turn instead of
    /// just executing the tool out-of-band. Any of the four may be `None`
    /// (e.g. a tenant turn with no session id) — a resume path must
    /// tolerate a missing `origin_message` by falling back to a generic
    /// continuation prompt, not by failing.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_with_context(
        &self,
        principal: &str,
        tool_name: &str,
        arguments: &str,
        risk_tier: &str,
        tenant_id: Option<&str>,
        agent_type: Option<&str>,
        session_id: Option<&str>,
        origin_message: Option<&str>,
    ) -> Result<String> {
        let id = format!("pa_{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO pending_approvals
                 (id, principal, tool_name, arguments, risk_tier, requested_at, status,
                  tenant_id, agent_type, session_id, origin_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10)",
            params![
                id,
                principal,
                tool_name,
                arguments,
                risk_tier,
                now,
                tenant_id,
                agent_type,
                session_id,
                origin_message
            ],
        )?;
        Ok(id)
    }

    /// Fetch one row by id, regardless of status.
    pub fn get(&self, id: &str) -> Result<Option<PendingApproval>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                    status, resolved_at, resolved_by,
                    tenant_id, agent_type, session_id, origin_message, delivered_at
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
                        status, resolved_at, resolved_by,
                        tenant_id, agent_type, session_id, origin_message, delivered_at
                   FROM pending_approvals WHERE status = ?1 ORDER BY requested_at DESC",
            )?
        } else {
            conn.prepare(
                "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                        status, resolved_at, resolved_by,
                        tenant_id, agent_type, session_id, origin_message, delivered_at
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
            tenant_id: r.get(9)?,
            agent_type: r.get(10)?,
            session_id: r.get(11)?,
            origin_message: r.get(12)?,
            delivered_at: r.get(13)?,
        })
    }

    /// Mark a resolved row's reply as delivered. Returns `true` if a row
    /// was actually updated (idempotent: marking an already-delivered row
    /// again is a harmless no-op — the reaper may race a live resolve call
    /// that delivers synchronously moments before a sweep picks up the same
    /// row, and both must be safe).
    pub fn mark_delivered(&self, id: &str) -> Result<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let updated = conn.execute(
            "UPDATE pending_approvals SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL",
            params![id, now],
        )?;
        Ok(updated == 1)
    }

    /// Cerveau (patch 0032): candidates for the reaper's redelivery sweep —
    /// resolved (`approved`/`denied`) rows with no `delivered_at` yet.
    /// Scoped to rows carrying `tenant_id` (patch 0028 context): a row with
    /// no tenant context was created by a non-tenant/loopback caller
    /// (`execute_approved_tool`'s own path), which never sets `delivered_at`
    /// in the first place and has no continuation/reply concept to redeliver.
    /// Newest-resolved-first is deliberate: a very recently resolved row is
    /// far more likely to be a live in-flight synchronous resolve the sweep
    /// would otherwise race pointlessly — callers should still apply their
    /// own age/grace-period filter on `resolved_at` before acting, this
    /// query only narrows the field.
    pub fn list_undelivered_resolved(&self) -> Result<Vec<PendingApproval>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, principal, tool_name, arguments, risk_tier, requested_at,
                    status, resolved_at, resolved_by,
                    tenant_id, agent_type, session_id, origin_message, delivered_at
               FROM pending_approvals
              WHERE status IN ('approved', 'denied')
                AND delivered_at IS NULL
                AND tenant_id IS NOT NULL
              ORDER BY resolved_at DESC",
        )?;
        stmt.query_map(params![], Self::row_to_pending_approval)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("list undelivered resolved pending_approvals")
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
        // Plain `insert` carries no origin context — a resume path must
        // treat this row as out-of-band-only.
        assert!(row.tenant_id.is_none());
        assert!(row.agent_type.is_none());
        assert!(row.session_id.is_none());
        assert!(row.origin_message.is_none());
    }

    #[test]
    fn insert_with_context_round_trips_all_four_fields() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store
            .insert_with_context(
                "u1",
                "finalize_invoice",
                "{}",
                "irreversible",
                Some("u1:finance_invoice_ops"),
                Some("finance_invoice_ops"),
                Some("sess-1"),
                Some("please finalize invoice inv_123"),
            )
            .unwrap();
        let row = store.get(&id).unwrap().expect("row must exist");
        assert_eq!(row.tenant_id.as_deref(), Some("u1:finance_invoice_ops"));
        assert_eq!(row.agent_type.as_deref(), Some("finance_invoice_ops"));
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            row.origin_message.as_deref(),
            Some("please finalize invoice inv_123")
        );
    }

    #[test]
    fn insert_with_context_tolerates_all_none_context() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store
            .insert_with_context("u1", "tool_a", "{}", "irreversible", None, None, None, None)
            .unwrap();
        let row = store.get(&id).unwrap().expect("row must exist");
        assert!(row.origin_message.is_none());
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

    #[test]
    fn new_rows_have_no_delivered_at() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store
            .insert_with_context(
                "u1",
                "tool_a",
                "{}",
                "irreversible",
                Some("u1.cs"),
                Some("customer_service"),
                None,
                None,
            )
            .unwrap();
        assert!(store.get(&id).unwrap().unwrap().delivered_at.is_none());
    }

    #[test]
    fn mark_delivered_sets_the_field_once_and_is_idempotent() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = store.insert("u1", "tool_a", "{}", "irreversible").unwrap();
        store.resolve(&id, "approved", "tenant-webhook").unwrap();

        assert!(store.mark_delivered(&id).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert!(row.delivered_at.is_some());

        // A second mark is a no-op, not an overwrite — proves the reaper
        // can safely race a synchronous resolve that delivers first.
        assert!(!store.mark_delivered(&id).unwrap());
        assert_eq!(store.get(&id).unwrap().unwrap().delivered_at, row.delivered_at);
    }

    #[test]
    fn list_undelivered_resolved_excludes_pending_delivered_and_no_tenant_context() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();

        // Candidate: resolved, tenant-scoped, never delivered.
        let candidate = store
            .insert_with_context(
                "u1",
                "tool_a",
                "{}",
                "irreversible",
                Some("u1.cs"),
                Some("customer_service"),
                Some("sess-1"),
                Some("hi"),
            )
            .unwrap();
        store.resolve(&candidate, "approved", "tenant-webhook").unwrap();

        // Not a candidate: still pending.
        store
            .insert_with_context(
                "u1",
                "tool_b",
                "{}",
                "irreversible",
                Some("u1.cs"),
                Some("customer_service"),
                None,
                None,
            )
            .unwrap();

        // Not a candidate: resolved AND already delivered.
        let delivered = store
            .insert_with_context(
                "u1",
                "tool_c",
                "{}",
                "irreversible",
                Some("u1.cs"),
                Some("customer_service"),
                None,
                None,
            )
            .unwrap();
        store.resolve(&delivered, "approved", "tenant-webhook").unwrap();
        store.mark_delivered(&delivered).unwrap();

        // Not a candidate: resolved but no tenant context (loopback/admin
        // path — no continuation/reply concept to redeliver).
        let no_tenant = store.insert("u1", "tool_d", "{}", "irreversible").unwrap();
        store.resolve(&no_tenant, "approved", "loopback-cli").unwrap();

        let candidates = store.list_undelivered_resolved().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, candidate);
    }
}
