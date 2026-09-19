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
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use parking_lot::Mutex;
use postgres::Client;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;
use uuid::Uuid;

/// A `done` task stops counting toward a tenant's live row scan after this
/// many days in the archive — see the module doc's "never deleted, only
/// archived" note just above: this bounds the archive itself, not the
/// ledger's audit-trail intent, which the archive already satisfies while a
/// row lives there.
const ARCHIVE_RETENTION_DAYS: i64 = 40;

/// How often the background sweep checks for expired archive rows. Cheap
/// query, generous interval — a 40-day cutoff never needs finer than this.
const ARCHIVE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Cap on archived rows returned in one `list_tasks` call — this is
/// operator/agent context, not a report; the newest ones are what matters.
const ARCHIVE_LIST_LIMIT: i64 = 20;

/// In-progress rows from another session older than this are orphans: no
/// live turn can still own them (turns last minutes, capped at 10 tool
/// iterations), while the dashboard flags stuck work much earlier — so the
/// sweep never races a running turn, it only buries the dead.
const ORPHAN_PARK_AFTER_MINUTES: i64 = 30;

/// What a status write actually did. `update_status` reports this instead of a
/// bare `()` so callers can tell the agent the truth: an operator-stopped task
/// silently ignores late agent writes (by design -- never a resurrection), and
/// "Task X is now done" for a write that changed nothing is a lie the agent then
/// acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusUpdate {
    /// The requested status was written.
    Applied,
    /// `done` was requested for a task that was already done: nothing changed,
    /// and the goal state holds.
    AlreadyDone,
    /// The operator stopped this task (dashboard Stop button); the write was
    /// ignored and the task stays cancelled.
    StoppedByOperator,
}

/// Cap on a delegation's `result_summary`. This is what a row and its archive
/// snapshot carry; the full text stays in the delegate result file.
const RESULT_SUMMARY_MAX_CHARS: usize = 2000;

fn truncate_summary(text: &str) -> String {
    if text.chars().count() <= RESULT_SUMMARY_MAX_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(RESULT_SUMMARY_MAX_CHARS).collect();
    format!("{cut}…")
}

/// Why a status write matched no *live* row. Distinguishing these matters
/// because the agent reads the error text: "not found" for a task that was
/// simply finished sends it hunting for a missing task and retrying, when the
/// truth is "this is done and terminal".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingTask {
    /// Operator-stopped: a late agent write is a silent no-op, never a resurrection.
    Cancelled,
    /// Finished earlier and moved to the archive: `done` is terminal.
    AlreadyDone,
    /// No such task for this tenant (or it belongs to another tenant -- the
    /// two are deliberately indistinguishable).
    NotFound,
}

impl MissingTask {
    fn from_flags(cancelled: bool, archived: bool) -> Self {
        if cancelled {
            Self::Cancelled
        } else if archived {
            Self::AlreadyDone
        } else {
            Self::NotFound
        }
    }
}

fn not_found_error(task_id: &str) -> anyhow::Error {
    anyhow::anyhow!("task {task_id} not found for this tenant")
}

fn already_done_error(task_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "task {task_id} is already done. `done` is terminal and cannot be changed or \
         re-opened; if there is new work, create a new task instead."
    )
}

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
    /// Operator-stopped via the dashboard Stop button (dashboard writes it
    /// directly — no agent tool accepts it). Terminal like `done`, but means
    /// "abandoned" rather than "delivered": the board hides it, the DB keeps
    /// it, and late agent writes to it are a silent no-op (see
    /// `update_status`). Reads surface it instead of mistaking it for
    /// `todo` — an agent that sees `cancelled` must not redo the work.
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(TaskStatus::Todo),
            "in_progress" => Some(TaskStatus::InProgress),
            "blocked" => Some(TaskStatus::Blocked),
            "done" => Some(TaskStatus::Done),
            "cancelled" => Some(TaskStatus::Cancelled),
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
    // ADR-014 A2: the link from a row to the delegation doing its work. All
    // optional and `#[serde(default)]`, so a gzip payload archived before these
    // fields existed still deserializes, and rows the agent creates itself
    // simply leave them empty.
    /// Groups related delegations (populated from ADR-014 P1; column exists now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Ledger row of the delegator, when known (reserved; not populated in A2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// `agent_type` that delegated this work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_by: Option<String>,
    /// The engine's delegation id (`TaskRegistry` / result-file task id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    /// Why a delegated row is `blocked` although the work is not waiting on a
    /// person: it failed, timed out, or was lost. `None` for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TaskOutcome>,
    /// Bounded summary of what the delegation produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
}

/// Terminal failure cause of a delegated row (ADR-014 §5.2). Deliberately not a
/// [`TaskStatus`]: the dashboard maps an unknown status to its `todo` column, so
/// a failed delegation is expressed as `blocked` plus one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Failed,
    TimedOut,
    Lost,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Failed => "failed",
            TaskOutcome::TimedOut => "timed_out",
            TaskOutcome::Lost => "lost",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "failed" => Some(TaskOutcome::Failed),
            "timed_out" => Some(TaskOutcome::TimedOut),
            "lost" => Some(TaskOutcome::Lost),
            _ => None,
        }
    }
}

/// A row created by the delegation engine rather than by an agent's own
/// `task_create`.
#[derive(Debug, Clone)]
pub struct NewDelegatedTask<'a> {
    pub tenant_id: &'a str,
    /// The specialist doing the work (the dashboard groups by exact `agent_type`).
    pub agent_type: &'a str,
    pub session_id: Option<&'a str>,
    pub title: &'a str,
    pub delegated_by: &'a str,
    pub delegation_id: &'a str,
    pub context_id: Option<&'a str>,
    /// `in_progress` for a background delegation that just started; `blocked`
    /// for a sync hop recorded after the fact because it needs a person.
    pub status: TaskStatus,
    pub outcome: Option<TaskOutcome>,
    pub blocked_reason: Option<&'a str>,
    pub result_summary: Option<&'a str>,
}

/// How a delegation ended, as the ledger needs to record it.
#[derive(Debug, Clone)]
pub enum DelegationEnd {
    /// Work finished; the row is archived like any `done` task, carrying the summary.
    Completed { summary: Option<String> },
    /// The sub-agent parked an irreversible action for a person.
    InputRequired { reason: String },
    /// Attempted and did not finish. Recorded as `blocked` + [`TaskOutcome`].
    Failed {
        outcome: TaskOutcome,
        reason: String,
    },
    /// Stopped by the operator or the caller.
    Cancelled { reason: String },
}

/// Result of pointing an existing row at a delegation (`ledger_task_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptResult {
    Adopted,
    /// Already linked to a delegation; one row cannot track two.
    AlreadyDelegated,
    /// Operator-stopped; never resurrected.
    Cancelled,
    /// Finished and archived; `done` is terminal.
    Done,
    /// No such row for this tenant (another tenant's id reads the same).
    NotFound,
}

/// Column list for reading an [`AgentTask`]; the order is what [`task_from_row`] expects.
const TASK_COLUMNS: &str = "task_id, tenant_id, agent_type, session_id, title, status, \
     priority, blocked_reason, created_at, updated_at, \
     context_id, parent_task_id, delegated_by, delegation_id, outcome, result_summary";

fn task_from_row(row: &postgres::Row) -> AgentTask {
    let status_raw: String = row.get(5);
    let priority_raw: String = row.get(6);
    let outcome_raw: Option<String> = row.get(14);
    AgentTask {
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
        context_id: row.get(10),
        parent_task_id: row.get(11),
        delegated_by: row.get(12),
        delegation_id: row.get(13),
        outcome: outcome_raw.as_deref().and_then(TaskOutcome::parse),
        result_summary: row.get(15),
    }
}

/// Columns `DELETE ... RETURNING` yields when a live row is archived; the order
/// is what [`archive_returned_row`] reads.
const ARCHIVE_RETURNING: &str = "task_id, tenant_id, agent_type, session_id, title, priority, \
     blocked_reason, created_at, context_id, parent_task_id, delegated_by, delegation_id, \
     outcome, result_summary";

/// Write the gzip-compressed `done` snapshot of a just-deleted live row into
/// `agent_tasks_archive`, inside the caller's transaction.
fn archive_returned_row(
    txn: &mut postgres::Transaction<'_>,
    schema: &str,
    row: &postgres::Row,
) -> Result<()> {
    let priority_raw: String = row.get(5);
    let outcome_raw: Option<String> = row.get(12);
    let context_id: Option<String> = row.get(8);
    let finished = AgentTask {
        task_id: row.get(0),
        tenant_id: row.get(1),
        agent_type: row.get(2),
        session_id: row.get(3),
        title: row.get(4),
        status: TaskStatus::Done,
        priority: TaskPriority::parse(&priority_raw).unwrap_or(TaskPriority::Normal),
        blocked_reason: None,
        created_at: row.get(7),
        updated_at: Utc::now(),
        context_id: context_id.clone(),
        parent_task_id: row.get(9),
        delegated_by: row.get(10),
        delegation_id: row.get(11),
        outcome: outcome_raw.as_deref().and_then(TaskOutcome::parse),
        result_summary: row.get(13),
    };

    let json = serde_json::to_vec(&finished).context("serialize finished task")?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json).context("gzip finished task")?;
    let payload = encoder.finish().context("finish gzip finished task")?;

    txn.execute(
        &format!(
            r#"INSERT INTO "{schema}".agent_tasks_archive
                   (task_id, tenant_id, agent_type, payload, context_id)
               VALUES ($1, $2, $3, $4, $5)"#
        ),
        &[
            &finished.task_id,
            &finished.tenant_id,
            &finished.agent_type,
            &payload,
            &context_id,
        ],
    )?;
    Ok(())
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

                CREATE TABLE IF NOT EXISTS "{schema_owned}".agent_tasks_archive (
                    task_id TEXT PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    agent_type TEXT NOT NULL,
                    archived_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    payload BYTEA NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_agent_tasks_archive_lookup
                    ON "{schema_owned}".agent_tasks_archive(tenant_id, agent_type, archived_at);

                -- ADR-014 A2: delegation link. Nullable and additive, so an older
                -- binary (explicit column lists everywhere) is unaffected.
                ALTER TABLE "{schema_owned}".agent_tasks
                    ADD COLUMN IF NOT EXISTS context_id TEXT,
                    ADD COLUMN IF NOT EXISTS parent_task_id TEXT,
                    ADD COLUMN IF NOT EXISTS delegated_by TEXT,
                    ADD COLUMN IF NOT EXISTS delegation_id TEXT,
                    ADD COLUMN IF NOT EXISTS outcome TEXT,
                    ADD COLUMN IF NOT EXISTS result_summary TEXT;
                -- Only delegated rows: cancel observation and the reconciler look
                -- rows up by delegation id, everything else never does.
                CREATE INDEX IF NOT EXISTS idx_agent_tasks_delegation
                    ON "{schema_owned}".agent_tasks(delegation_id)
                    WHERE delegation_id IS NOT NULL;
                ALTER TABLE "{schema_owned}".agent_tasks_archive
                    ADD COLUMN IF NOT EXISTS context_id TEXT;
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
    /// Orphan sweep: park `in_progress` rows whose turn is gone.
    ///
    /// A row is orphaned when it belongs to a DIFFERENT session than the
    /// calling turn and hasn't moved for longer than
    /// `ORPHAN_PARK_AFTER_MINUTES`. Same-session rows are never touched
    /// (the owner may still be running or about to continue), `todo` rows
    /// are plans not orphans, and `blocked` rows already surface in Waiting
    /// with the operator Stop button — only `in_progress` can lie about
    /// running. Parked rows keep their history; the operator or a resumed
    /// turn can still finish them.
    ///
    /// Called piggyback on every ledger tool execution (create/list/update),
    /// so no turn needs to remember it — and turns that never touch the
    /// ledger cannot orphan anything. Returns the number parked.
    ///
    /// Rows that belong to a delegation (`delegation_id IS NOT NULL`, ADR-014)
    /// are never parked here: a background delegation legitimately runs in a
    /// different session for longer than this window, and its row is settled by
    /// the delegation itself, or by the reaper's reconciler if the daemon died.
    ///
    /// [`ORPHAN_PARK_AFTER_MINUTES`] bounds the sweep: well under the
    /// dashboard's stuck flag, well over any single turn's lifetime, so a
    /// live turn's rows (same session, or minutes fresh) can never match.
    pub async fn park_orphaned_tasks(
        &self,
        tenant_id: &str,
        agent_type: &str,
        current_session: Option<&str>,
    ) -> Result<u64> {
        let Some(session) = current_session else {
            return Ok(0);
        };
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        let session = session.to_string();
        let older_than_minutes = ORPHAN_PARK_AFTER_MINUTES.to_string();
        run_on_os_thread(move || -> Result<u64> {
            let mut client = client.lock();
            let rows = client.execute(
                &format!(
                    r#"UPDATE "{schema}".agent_tasks
                       SET status = 'blocked',
                           blocked_reason = 'Orphaned: its turn ended without closing it; parked automatically. Reply in this thread to resume it, or Stop it from Mission Control.',
                           updated_at = NOW()
                       WHERE tenant_id = $1 AND agent_type = $2
                         AND status = 'in_progress'
                         AND delegation_id IS NULL
                         AND session_id IS DISTINCT FROM $3
                         AND updated_at < NOW() - ($4 || ' minutes')::interval"#
                ),
                &[&tenant_id, &agent_type, &session, &older_than_minutes],
            )?;
            Ok(rows)
        })
        .await
    }

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
    ///
    /// `Done` is not a resting state in `agent_tasks` — see
    /// `archive_task_transactionally` below. Every other status is a plain
    /// in-place `UPDATE`.
    pub async fn update_status(
        &self,
        tenant_id: &str,
        task_id: &str,
        status: TaskStatus,
        blocked_reason: Option<&str>,
    ) -> Result<StatusUpdate> {
        if status == TaskStatus::Done {
            return self.archive_task_transactionally(tenant_id, task_id).await;
        }

        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let task_id_owned = task_id.to_string();
        let task_id_for_error = task_id_owned.clone();
        let status_str = status.as_str().to_string();
        let blocked_reason = blocked_reason.map(str::to_string);
        // Clones for the diagnosis below (the update closure moves the
        // originals onto its OS thread).
        let tenant_probe = tenant_id.clone();
        let task_probe = task_id_owned.clone();
        let schema_probe = schema.clone();
        let client_probe = Arc::clone(self.client.get());
        let rows = run_on_os_thread(move || -> Result<u64> {
            let mut client = client.lock();
            let rows = client.execute(
                &format!(
                    r#"UPDATE "{schema}".agent_tasks
                       SET status = $1, blocked_reason = $2, updated_at = NOW()
                       WHERE task_id = $3 AND tenant_id = $4 AND status <> 'cancelled'"#
                ),
                &[&status_str, &blocked_reason, &task_id_owned, &tenant_id],
            )?;
            Ok(rows)
        })
        .await?;
        if rows == 0 {
            let missing = run_on_os_thread(move || -> Result<MissingTask> {
                let mut client = client_probe.lock();
                let row = client.query_one(
                    &format!(
                        r#"SELECT
                             EXISTS (SELECT 1 FROM "{schema_probe}".agent_tasks
                                     WHERE task_id = $1 AND tenant_id = $2
                                       AND status = 'cancelled'),
                             EXISTS (SELECT 1 FROM "{schema_probe}".agent_tasks_archive
                                     WHERE task_id = $1 AND tenant_id = $2)"#
                    ),
                    &[&task_probe, &tenant_probe],
                )?;
                Ok(MissingTask::from_flags(row.get(0), row.get(1)))
            })
            .await?;
            return match missing {
                // A task the operator stopped stays stopped: a late agent
                // write is ignored, never a resurrection -- but reported.
                MissingTask::Cancelled => Ok(StatusUpdate::StoppedByOperator),
                MissingTask::AlreadyDone => Err(already_done_error(&task_id_for_error)),
                MissingTask::NotFound => Err(not_found_error(&task_id_for_error)),
            };
        }
        Ok(StatusUpdate::Applied)
    }

    /// `done` transition: move the row out of `agent_tasks` into
    /// `agent_tasks_archive` as one gzip-compressed JSON blob, atomically
    /// (one Postgres transaction — a crash between a delete and an insert
    /// must never duplicate or lose the task). Keeps the live table small
    /// and fast (it only ever holds open work) while the ledger's own
    /// audit-trail guarantee still holds: nothing is lost, `list_tasks`
    /// still surfaces it, just from the archive.
    async fn archive_task_transactionally(
        &self,
        tenant_id: &str,
        task_id: &str,
    ) -> Result<StatusUpdate> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let task_id_owned = task_id.to_string();
        let task_id_for_error = task_id_owned.clone();
        run_on_os_thread(move || -> Result<StatusUpdate> {
            let mut client = client.lock();
            let mut txn = client.transaction()?;

            let row = txn.query_opt(
                &format!(
                    r#"DELETE FROM "{schema}".agent_tasks
                       WHERE task_id = $1 AND tenant_id = $2 AND status <> 'cancelled'
                       RETURNING {ARCHIVE_RETURNING}"#
                ),
                &[&task_id_owned, &tenant_id],
            )?;
            let Some(row) = row else {
                // Nothing live to finish. Diagnose why, in one round trip:
                //  - operator-cancelled: a late agent `done` is a silent no-op.
                //    The DELETE above deliberately skips cancelled rows;
                //    archiving one would rewrite "abandoned" as "delivered" and
                //    resurrect it on the board.
                //  - already archived: the goal state already holds, succeed
                //    idempotently rather than tell the agent a finished task
                //    does not exist.
                //  - neither: genuinely unknown. Both probes are tenant-scoped,
                //    so another tenant's id still reads as not found.
                let flags = txn.query_one(
                    &format!(
                        r#"SELECT
                             EXISTS (SELECT 1 FROM "{schema}".agent_tasks
                                     WHERE task_id = $1 AND tenant_id = $2
                                       AND status = 'cancelled'),
                             EXISTS (SELECT 1 FROM "{schema}".agent_tasks_archive
                                     WHERE task_id = $1 AND tenant_id = $2)"#
                    ),
                    &[&task_id_owned, &tenant_id],
                )?;
                return match MissingTask::from_flags(flags.get(0), flags.get(1)) {
                    MissingTask::Cancelled => Ok(StatusUpdate::StoppedByOperator),
                    MissingTask::AlreadyDone => Ok(StatusUpdate::AlreadyDone),
                    MissingTask::NotFound => Err(not_found_error(&task_id_for_error)),
                };
            };

            archive_returned_row(&mut txn, &schema, &row)?;

            txn.commit()?;
            Ok(StatusUpdate::Applied)
        })
        .await
    }

    /// Archived (already-`done`) tasks for a tenant+agent-type, newest
    /// first, decompressed back into `AgentTask`. A row whose payload fails
    /// to decompress/parse is dropped rather than failing the whole call —
    /// one corrupt archive entry must not hide every other task from
    /// `task_list`.
    async fn list_archived_tasks(
        &self,
        tenant_id: &str,
        agent_type: &str,
    ) -> Result<Vec<AgentTask>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        run_on_os_thread(move || -> Result<Vec<AgentTask>> {
            let mut client = client.lock();
            let rows = client.query(
                &format!(
                    r#"SELECT payload FROM "{schema}".agent_tasks_archive
                       WHERE tenant_id = $1 AND agent_type = $2
                       ORDER BY archived_at DESC
                       LIMIT {ARCHIVE_LIST_LIMIT}"#
                ),
                &[&tenant_id, &agent_type],
            )?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let payload: Vec<u8> = row.get(0);
                let mut decoder = GzDecoder::new(payload.as_slice());
                let mut json = Vec::new();
                if decoder.read_to_end(&mut json).is_err() {
                    continue;
                }
                if let Ok(task) = serde_json::from_slice::<AgentTask>(&json) {
                    out.push(task);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Permanently delete archived tasks older than [`ARCHIVE_RETENTION_DAYS`].
    /// Best-effort, called on a fixed interval by [`Self::spawn_archive_sweep`]
    /// — a failed sweep just tries again next tick, same posture as the
    /// keep-warm pings elsewhere in Cerveau.
    async fn sweep_expired_archive(&self) -> Result<u64> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        run_on_os_thread(move || -> Result<u64> {
            let mut client = client.lock();
            let rows = client.execute(
                &format!(
                    r#"DELETE FROM "{schema}".agent_tasks_archive
                       WHERE archived_at < NOW() - INTERVAL '{ARCHIVE_RETENTION_DAYS} days'"#
                ),
                &[],
            )?;
            Ok(rows)
        })
        .await
    }

    /// Start the background 40-day archive-retention sweep. Call once, right
    /// after installing the process-wide ledger (see `install_task_ledger`'s
    /// caller in `main.rs`) — idempotent only in the sense that the caller
    /// is expected to call it once; a second call would just run a second,
    /// harmless, redundant sweep loop.
    pub fn spawn_archive_sweep(self: &Arc<Self>) {
        let ledger = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ARCHIVE_SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                match ledger.sweep_expired_archive().await {
                    Ok(deleted) if deleted > 0 => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({ "deleted": deleted })),
                            "agent_tasks_archive: swept expired rows"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_attrs(::serde_json::json!({ "error": e.to_string() })),
                            "agent_tasks_archive: sweep failed, will retry next interval"
                        );
                    }
                }
            }
        });
    }

    // ── ADR-014 A2: delegation link ───────────────────────────────────────
    //
    // The row is a *projection* of a delegation: the engine owns execution
    // state (result file + `TaskRegistry`) and mirrors it here for humans. These
    // methods are engine-internal and keyed by `delegation_id`, an engine-minted
    // UUID that never comes from model output; only `adopt_for_delegation`
    // takes a model-supplied id, and it is tenant-scoped like every other read.

    /// Create the row for a delegation. Returns the new `task_id`.
    pub async fn create_delegated_task(&self, new: NewDelegatedTask<'_>) -> Result<String> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let task_id = Uuid::new_v4().to_string();
        let id_out = task_id.clone();
        let tenant_id = new.tenant_id.to_string();
        let agent_type = new.agent_type.to_string();
        let session_id = new.session_id.map(str::to_string);
        let title = new.title.to_string();
        let status = new.status.as_str().to_string();
        let blocked_reason = new.blocked_reason.map(str::to_string);
        let context_id = new.context_id.map(str::to_string);
        let delegated_by = new.delegated_by.to_string();
        let delegation_id = new.delegation_id.to_string();
        let outcome = new.outcome.map(|o| o.as_str().to_string());
        let result_summary = new.result_summary.map(truncate_summary);
        run_on_os_thread(move || -> Result<()> {
            let mut client = client.lock();
            client.execute(
                &format!(
                    r#"INSERT INTO "{schema}".agent_tasks
                           (task_id, tenant_id, agent_type, session_id, title, status,
                            blocked_reason, context_id, delegated_by, delegation_id,
                            outcome, result_summary)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#
                ),
                &[
                    &task_id,
                    &tenant_id,
                    &agent_type,
                    &session_id,
                    &title,
                    &status,
                    &blocked_reason,
                    &context_id,
                    &delegated_by,
                    &delegation_id,
                    &outcome,
                    &result_summary,
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(id_out)
    }

    /// Point an existing, still-open row at a delegation (the `ledger_task_id`
    /// path), so the row the caller already created is the one that tracks the
    /// work instead of a duplicate appearing beside it.
    pub async fn adopt_for_delegation(
        &self,
        tenant_id: &str,
        task_id: &str,
        delegation_id: &str,
        delegated_by: &str,
        context_id: Option<&str>,
    ) -> Result<AdoptResult> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let task_id = task_id.to_string();
        let delegation_id = delegation_id.to_string();
        let delegated_by = delegated_by.to_string();
        let context_id = context_id.map(str::to_string);
        run_on_os_thread(move || -> Result<AdoptResult> {
            let mut client = client.lock();
            let updated = client.execute(
                &format!(
                    r#"UPDATE "{schema}".agent_tasks
                       SET delegation_id = $3, delegated_by = $4, context_id = $5,
                           status = 'in_progress', blocked_reason = NULL, outcome = NULL,
                           updated_at = NOW()
                       WHERE task_id = $1 AND tenant_id = $2
                         AND delegation_id IS NULL
                         AND status IN ('todo', 'in_progress', 'blocked')"#
                ),
                &[
                    &task_id,
                    &tenant_id,
                    &delegation_id,
                    &delegated_by,
                    &context_id,
                ],
            )?;
            if updated == 1 {
                return Ok(AdoptResult::Adopted);
            }
            // Not adopted: say why, tenant-scoped so a foreign id reads as not found.
            let live = client.query_opt(
                &format!(
                    r#"SELECT status, delegation_id FROM "{schema}".agent_tasks
                       WHERE task_id = $1 AND tenant_id = $2"#
                ),
                &[&task_id, &tenant_id],
            )?;
            if let Some(row) = live {
                let status: String = row.get(0);
                let existing: Option<String> = row.get(1);
                return Ok(if status == "cancelled" {
                    AdoptResult::Cancelled
                } else if existing.is_some() {
                    AdoptResult::AlreadyDelegated
                } else {
                    AdoptResult::NotFound
                });
            }
            let archived: bool = client
                .query_one(
                    &format!(
                        r#"SELECT EXISTS (SELECT 1 FROM "{schema}".agent_tasks_archive
                                          WHERE task_id = $1 AND tenant_id = $2)"#
                    ),
                    &[&task_id, &tenant_id],
                )?
                .get(0);
            Ok(if archived {
                AdoptResult::Done
            } else {
                AdoptResult::NotFound
            })
        })
        .await
    }

    /// Record how a delegation ended on its row. Returns `false` when there is
    /// nothing to update: no row for this delegation, or the operator already
    /// stopped it (a late write never turns "abandoned" into anything else).
    /// Idempotent, so the engine and the reconciler may both call it.
    pub async fn finish_delegation(&self, delegation_id: &str, end: DelegationEnd) -> Result<bool> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let delegation_id = delegation_id.to_string();
        run_on_os_thread(move || -> Result<bool> {
            let mut client = client.lock();
            match end {
                DelegationEnd::Completed { summary } => {
                    let summary = summary.as_deref().map(truncate_summary);
                    let mut txn = client.transaction()?;
                    let updated = txn.execute(
                        &format!(
                            r#"UPDATE "{schema}".agent_tasks
                               SET result_summary = $2, updated_at = NOW()
                               WHERE delegation_id = $1 AND status <> 'cancelled'"#
                        ),
                        &[&delegation_id, &summary],
                    )?;
                    if updated == 0 {
                        return Ok(false);
                    }
                    let row = txn.query_opt(
                        &format!(
                            r#"DELETE FROM "{schema}".agent_tasks
                               WHERE delegation_id = $1 AND status <> 'cancelled'
                               RETURNING {ARCHIVE_RETURNING}"#
                        ),
                        &[&delegation_id],
                    )?;
                    let Some(row) = row else {
                        return Ok(false);
                    };
                    archive_returned_row(&mut txn, &schema, &row)?;
                    txn.commit()?;
                    Ok(true)
                }
                DelegationEnd::InputRequired { reason } => Ok(client.execute(
                    &format!(
                        r#"UPDATE "{schema}".agent_tasks
                           SET status = 'blocked', blocked_reason = $2, outcome = NULL,
                               updated_at = NOW()
                           WHERE delegation_id = $1 AND status <> 'cancelled'"#
                    ),
                    &[&delegation_id, &reason],
                )? > 0),
                DelegationEnd::Failed { outcome, reason } => {
                    let outcome = outcome.as_str().to_string();
                    Ok(client.execute(
                        &format!(
                            r#"UPDATE "{schema}".agent_tasks
                               SET status = 'blocked', outcome = $2, blocked_reason = $3,
                                   updated_at = NOW()
                               WHERE delegation_id = $1 AND status <> 'cancelled'"#
                        ),
                        &[&delegation_id, &outcome, &reason],
                    )? > 0)
                }
                DelegationEnd::Cancelled { reason } => Ok(client.execute(
                    &format!(
                        r#"UPDATE "{schema}".agent_tasks
                           SET status = 'cancelled', blocked_reason = $2, updated_at = NOW()
                           WHERE delegation_id = $1 AND status <> 'cancelled'"#
                    ),
                    &[&delegation_id, &reason],
                )? > 0),
            }
        })
        .await
    }

    /// Current status of the live row for a delegation; `None` once the row is
    /// gone (finished and archived) or never existed. This is how a running
    /// delegation notices the operator pressed Stop.
    pub async fn delegation_status(&self, delegation_id: &str) -> Result<Option<TaskStatus>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let delegation_id = delegation_id.to_string();
        run_on_os_thread(move || -> Result<Option<TaskStatus>> {
            let mut client = client.lock();
            let row = client.query_opt(
                &format!(r#"SELECT status FROM "{schema}".agent_tasks WHERE delegation_id = $1"#),
                &[&delegation_id],
            )?;
            Ok(row.and_then(|row| {
                let status: String = row.get(0);
                TaskStatus::parse(&status)
            }))
        })
        .await
    }

    /// Delegation ids of rows still shown as `in_progress`, for the reconciler
    /// that settles rows whose delegation ended without the engine writing it
    /// (a daemon restart, for one). Served by the partial delegation index.
    pub async fn open_delegation_ids(&self) -> Result<Vec<String>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        run_on_os_thread(move || -> Result<Vec<String>> {
            let mut client = client.lock();
            let rows = client.query(
                &format!(
                    r#"SELECT delegation_id FROM "{schema}".agent_tasks
                       WHERE delegation_id IS NOT NULL AND status = 'in_progress'"#
                ),
                &[],
            )?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
        .await
    }

    /// Fetch one task by id, scoped to `tenant_id` so one tenant can never
    /// read another's row by guessing a UUID. `None` if it doesn't exist or
    /// belongs to a different tenant -- same not-found shape either way, so
    /// this can't be used to probe for other tenants' task ids.
    pub async fn get_task(&self, tenant_id: &str, task_id: &str) -> Result<Option<AgentTask>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let task_id = task_id.to_string();
        run_on_os_thread(move || -> Result<Option<AgentTask>> {
            let mut client = client.lock();
            let row = client.query_opt(
                &format!(
                    r#"SELECT {TASK_COLUMNS}
                       FROM "{schema}".agent_tasks
                       WHERE task_id = $1 AND tenant_id = $2"#
                ),
                &[&task_id, &tenant_id],
            )?;
            Ok(row.map(|row| task_from_row(&row)))
        })
        .await
    }

    /// List a tenant+agent-type's tasks, optionally filtered to one status.
    /// Ordered newest-updated-first so an agent's most recent state change
    /// surfaces first at session start.
    ///
    /// `done` tasks no longer live in `agent_tasks` (see
    /// `archive_task_transactionally`), so a `Some(Done)` filter — or no
    /// filter at all, which must still be able to surface a recently
    /// finished task — reads them back from `agent_tasks_archive` instead
    /// (or in addition, for no filter, newest-first across both).
    pub async fn list_tasks(
        &self,
        tenant_id: &str,
        agent_type: &str,
        status: Option<TaskStatus>,
    ) -> Result<Vec<AgentTask>> {
        if status == Some(TaskStatus::Done) {
            return self.list_archived_tasks(tenant_id, agent_type).await;
        }

        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id_owned = tenant_id.to_string();
        let agent_type_owned = agent_type.to_string();
        let tenant_id = tenant_id_owned.clone();
        let agent_type = agent_type_owned.clone();
        let status_str = status.map(|s| s.as_str().to_string());
        let mut out = run_on_os_thread(move || -> Result<Vec<AgentTask>> {
            let mut client = client.lock();
            let rows = match &status_str {
                Some(s) => client.query(
                    &format!(
                        r#"SELECT {TASK_COLUMNS}
                           FROM "{schema}".agent_tasks
                           WHERE tenant_id = $1 AND agent_type = $2 AND status = $3
                           ORDER BY updated_at DESC"#
                    ),
                    &[&tenant_id, &agent_type, s],
                )?,
                None => client.query(
                    &format!(
                        r#"SELECT {TASK_COLUMNS}
                           FROM "{schema}".agent_tasks
                           WHERE tenant_id = $1 AND agent_type = $2
                           ORDER BY updated_at DESC"#
                    ),
                    &[&tenant_id, &agent_type],
                )?,
            };
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(task_from_row(&row));
            }
            Ok(out)
        })
        .await?;

        // No filter: an agent listing "everything" should still see recently
        // finished work, not just what's still open.
        if status.is_none() {
            out.extend(
                self.list_archived_tasks(&tenant_id_owned, &agent_type_owned)
                    .await?,
            );
            out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }

        Ok(out)
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
    use super::{MissingTask, already_done_error, not_found_error};

    #[test]
    fn missing_task_diagnosis_prefers_cancelled_then_archived_then_not_found() {
        assert_eq!(MissingTask::from_flags(true, false), MissingTask::Cancelled);
        assert_eq!(MissingTask::from_flags(true, true), MissingTask::Cancelled);
        assert_eq!(
            MissingTask::from_flags(false, true),
            MissingTask::AlreadyDone
        );
        assert_eq!(MissingTask::from_flags(false, false), MissingTask::NotFound);
    }

    #[test]
    fn already_done_error_says_done_is_terminal_not_missing() {
        let done = already_done_error("t-1").to_string();
        assert!(
            done.contains("t-1") && done.contains("already done"),
            "{done}"
        );
        assert!(
            done.contains("terminal") && done.contains("create a new task"),
            "{done}"
        );
        assert!(
            !done.contains("not found"),
            "must not read as a missing task: {done}"
        );
        assert_eq!(
            not_found_error("t-1").to_string(),
            "task t-1 not found for this tenant"
        );
    }

    use super::*;

    #[test]
    fn status_round_trips() {
        for s in [
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Cancelled,
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
