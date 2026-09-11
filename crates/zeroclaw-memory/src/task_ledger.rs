//! Aivory Cerveau: Agent Task Ledger (`agent_tasks` table).
//!
//! A deterministic, per-tenant to-do list an agent keeps on itself —
//! separate from `cognee-rs` graph memory (`graph_memory.rs`, see
//! docs/ADR-007-CERVEAU-COGNEE-INTEGRATION.md in AVRY-V2-Main). Graph
//! recall answers "what do I know"; this answers "what was I in the
//! middle of" with a plain row scan, not a semantic search, so it can't
//! miss something that's actually there. See the Agent Task Ledger
//! PRD/TRD for the full motivation.
//!
//! Same connection shape as [`crate::capability_graph::PgCapabilityGraph`]:
//! its own `Arc<Mutex<postgres::Client>>`, connect-and-init-schema in one
//! continuous blocking call on a dedicated spawned OS thread (splitting
//! connect and schema-init into separate `run_on_os_thread` calls hit a
//! real "Cannot start a runtime from within a runtime" panic — see that
//! module's `connect` doc comment), every later operation on its own fresh
//! thread, and a `DropOnThread` wrapper so the final drop of
//! `postgres::Client` (which calls `Runtime::block_on` internally) never
//! panics from inside a Tokio runtime thread.
//!
//! A task is never deleted, only moved to `done` — the ledger doubles as
//! its own audit trail.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use postgres::Client;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Lifecycle state of a task. Terminal state is `Done` — re-opening a done
/// task is deliberately not supported; the clean way to correct a mistake
/// after the fact is a new task referencing the old one, not mutating
/// history (see the PRD's open-questions section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Blocked,
    Done,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(TaskStatus::Todo),
            "in_progress" => Some(TaskStatus::InProgress),
            "blocked" => Some(TaskStatus::Blocked),
            "done" => Some(TaskStatus::Done),
            _ => None,
        }
    }
}

/// Priority an agent assigns a task at creation time. Purely informational
/// today — nothing schedules or reorders on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
}

impl TaskPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskPriority::Low => "low",
            TaskPriority::Normal => "normal",
            TaskPriority::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(TaskPriority::Low),
            "normal" => Some(TaskPriority::Normal),
            "high" => Some(TaskPriority::High),
            _ => None,
        }
    }
}

/// One row of the ledger, as read back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub task_id: String,
    pub tenant_id: String,
    pub agent_type: String,
    pub session_id: Option<String>,
    pub title: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub blocked_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

async fn run_on_os_thread<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("pg-task-ledger-op".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("failed to spawn pg task ledger thread")?;
    rx.await.map_err(|_| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "pg task ledger thread terminated unexpectedly"
        );
        anyhow::Error::msg("pg task ledger thread terminated unexpectedly")
    })?
}

/// Drops its inner value on a background OS thread — see
/// `capability_graph::DropOnThread` (duplicated, not shared, same
/// small-connection-thread-bridge precedent as `run_on_os_thread` above).
struct DropOnThread<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> DropOnThread<T> {
    fn new(value: T) -> Self {
        Self(Some(value))
    }
    fn get(&self) -> &T {
        self.0.as_ref().expect("DropOnThread value already taken")
    }
}

impl<T: Send + 'static> Drop for DropOnThread<T> {
    fn drop(&mut self) {
        let Some(value) = self.0.take() else { return };
        let slot = std::mem::ManuallyDrop::new(value);
        if std::thread::Builder::new()
            .name("pg-task-ledger-drop".to_string())
            .spawn(move || drop(std::mem::ManuallyDrop::into_inner(slot)))
            .is_err()
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "pg-task-ledger-drop thread spawn failed; leaking client to avoid nested-runtime panic"
            );
        }
    }
}

/// Postgres-backed agent task ledger. Own connection, independent of
/// whatever `PostgresMemory`/`PgCapabilityGraph` instance the daemon may
/// also hold — same tradeoff `PgCapabilityGraph` makes (one extra
/// long-lived connection, well inside the tuned 200-connection budget).
pub struct AgentTaskLedger {
    client: DropOnThread<Arc<Mutex<Client>>>,
    schema: String,
}

impl AgentTaskLedger {
    /// Opens a new connection and ensures the table exists. `db_url` is the
    /// same libpq key=value DSN every other Cerveau Postgres consumer uses
    /// (NOT a `postgresql://` URL — see CERVEAU-STATUS.md §7).
    pub async fn connect(db_url: &str, schema: &str) -> Result<Self> {
        let db_url = db_url.to_string();
        let schema_owned = schema.to_string();
        // Connect AND create-table-if-missing on the SAME spawned OS thread,
        // in one continuous synchronous call chain — see
        // `PgCapabilityGraph::connect`'s doc comment for why splitting this
        // into two separate `run_on_os_thread` calls is unsafe (a real
        // "Cannot start a runtime from within a runtime" panic, reproduced
        // in CI).
        let client = run_on_os_thread(move || -> Result<Client> {
            let mut client =
                Client::connect(&db_url, postgres::NoTls).context("connect to Postgres")?;
            client.batch_execute(&format!(
                r#"
                CREATE TABLE IF NOT EXISTS "{schema_owned}".agent_tasks (
                    task_id TEXT PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    agent_type TEXT NOT NULL,
                    session_id TEXT,
                    title TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'todo',
                    priority TEXT NOT NULL DEFAULT 'normal',
                    blocked_reason TEXT,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );
                CREATE INDEX IF NOT EXISTS idx_agent_tasks_lookup
                    ON "{schema_owned}".agent_tasks(tenant_id, agent_type, status);
                "#
            ))?;
            Ok(client)
        })
        .await?;
        Ok(Self {
            client: DropOnThread::new(Arc::new(Mutex::new(client))),
            schema: schema.to_string(),
        })
    }

    /// Create a task in `todo`. Returns the generated `task_id`.
    pub async fn create_task(
        &self,
        tenant_id: &str,
        agent_type: &str,
        session_id: Option<&str>,
        title: &str,
        priority: TaskPriority,
    ) -> Result<String> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let task_id = Uuid::new_v4().to_string();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        let session_id = session_id.map(str::to_string);
        let title = title.to_string();
        let priority_str = priority.as_str().to_string();
        let id_out = task_id.clone();
        run_on_os_thread(move || -> Result<()> {
            let mut client = client.lock();
            client.execute(
                &format!(
                    r#"INSERT INTO "{schema}".agent_tasks
                           (task_id, tenant_id, agent_type, session_id, title, status, priority)
                       VALUES ($1, $2, $3, $4, $5, 'todo', $6)"#
                ),
                &[
                    &task_id,
                    &tenant_id,
                    &agent_type,
                    &session_id,
                    &title,
                    &priority_str,
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(id_out)
    }

    /// Move a task to a new status. `blocked_reason` is required by the
    /// caller (the tool layer) when `status == Blocked`; this method just
    /// stores whatever it's given. Errors if `task_id` doesn't belong to
    /// `tenant_id` (or doesn't exist), so one tenant's agent can never
    /// touch another tenant's row even by guessing a UUID.
    pub async fn update_status(
        &self,
        tenant_id: &str,
        task_id: &str,
        status: TaskStatus,
        blocked_reason: Option<&str>,
    ) -> Result<()> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let task_id_owned = task_id.to_string();
        let task_id_for_error = task_id_owned.clone();
        let status_str = status.as_str().to_string();
        let blocked_reason = blocked_reason.map(str::to_string);
        let rows = run_on_os_thread(move || -> Result<u64> {
            let mut client = client.lock();
            let rows = client.execute(
                &format!(
                    r#"UPDATE "{schema}".agent_tasks
                       SET status = $1, blocked_reason = $2, updated_at = NOW()
                       WHERE task_id = $3 AND tenant_id = $4"#
                ),
                &[&status_str, &blocked_reason, &task_id_owned, &tenant_id],
            )?;
            Ok(rows)
        })
        .await?;
        if rows == 0 {
            anyhow::bail!("task {task_id_for_error} not found for this tenant");
        }
        Ok(())
    }

    /// List a tenant+agent-type's tasks, optionally filtered to one status.
    /// Ordered newest-updated-first so an agent's most recent state change
    /// surfaces first at session start.
    pub async fn list_tasks(
        &self,
        tenant_id: &str,
        agent_type: &str,
        status: Option<TaskStatus>,
    ) -> Result<Vec<AgentTask>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        let status_str = status.map(|s| s.as_str().to_string());
        run_on_os_thread(move || -> Result<Vec<AgentTask>> {
            let mut client = client.lock();
            let rows = match &status_str {
                Some(s) => client.query(
                    &format!(
                        r#"SELECT task_id, tenant_id, agent_type, session_id, title, status,
                                  priority, blocked_reason, created_at, updated_at
                           FROM "{schema}".agent_tasks
                           WHERE tenant_id = $1 AND agent_type = $2 AND status = $3
                           ORDER BY updated_at DESC"#
                    ),
                    &[&tenant_id, &agent_type, s],
                )?,
                None => client.query(
                    &format!(
                        r#"SELECT task_id, tenant_id, agent_type, session_id, title, status,
                                  priority, blocked_reason, created_at, updated_at
                           FROM "{schema}".agent_tasks
                           WHERE tenant_id = $1 AND agent_type = $2
                           ORDER BY updated_at DESC"#
                    ),
                    &[&tenant_id, &agent_type],
                )?,
            };
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let status_raw: String = row.get(5);
                let priority_raw: String = row.get(6);
                out.push(AgentTask {
                    task_id: row.get(0),
                    tenant_id: row.get(1),
                    agent_type: row.get(2),
                    session_id: row.get(3),
                    title: row.get(4),
                    status: TaskStatus::parse(&status_raw).unwrap_or(TaskStatus::Todo),
                    priority: TaskPriority::parse(&priority_raw).unwrap_or(TaskPriority::Normal),
                    blocked_reason: row.get(7),
                    created_at: row.get(8),
                    updated_at: row.get(9),
                });
            }
            Ok(out)
        })
        .await
    }
}

// ── Process-wide install hook ────────────────────────────────────────────
//
// Same "install once at daemon startup, absent ⇒ None ⇒ tools simply don't
// get constructed" shape as `capability_graph`'s singleton — see
// `zeroclaw-runtime::tools::all_tools_with_runtime` for the construction
// site that reads this back.
static TASK_LEDGER: OnceLock<Arc<AgentTaskLedger>> = OnceLock::new();

/// Register the process-wide task ledger. Idempotent — only the first call
/// takes effect.
pub fn install_task_ledger(ledger: Arc<AgentTaskLedger>) {
    let _ = TASK_LEDGER.set(ledger);
}

/// The installed task ledger, if any. `None` when `[agent_tasks].enabled =
/// false`, the active memory backend isn't Postgres, or before daemon
/// startup has run its installation step (e.g. in unit tests).
pub fn current_task_ledger() -> Option<Arc<AgentTaskLedger>> {
    TASK_LEDGER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for s in [
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::Blocked,
            TaskStatus::Done,
        ] {
            assert_eq!(TaskStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(TaskStatus::parse("bogus"), None);
    }

    #[test]
    fn priority_round_trips() {
        for p in [TaskPriority::Low, TaskPriority::Normal, TaskPriority::High] {
            assert_eq!(TaskPriority::parse(p.as_str()), Some(p));
        }
        assert_eq!(TaskPriority::parse("bogus"), None);
    }
}
