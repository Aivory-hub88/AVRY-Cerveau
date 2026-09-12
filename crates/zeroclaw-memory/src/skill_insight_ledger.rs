//! Aivory Cerveau: Skill Insight Ledger (`skill_insights` table) — ADR-013
//! Phase 1.
//!
//! A cheap, per-tenant record of turns where something went wrong for the
//! agent (a tool call failed, or it had to escalate to a human) — the
//! "cheap gate" stage of a tenant-facing self-evolution pipeline, applying
//! the same layered cheap-guardrail/expensive-judge principle ADR-008
//! already uses elsewhere. Phase 1 is observation only: rows are written
//! here and nothing else consumes them yet (no LLM synthesis, no skill
//! generation) — the point is to learn how often these signals actually
//! fire before spending anything more on them.
//!
//! Deliberately its own table, not folded into
//! [`crate::task_ledger::AgentTaskLedger`]'s `agent_tasks` — the two have
//! different lifecycles and consumers (see ADR-013 §3 Stage 2). What *is*
//! reused from that ledger, line for line: `TEXT` status columns with
//! Rust-side `as_str()`/`parse()`, a single tenant-scoped lookup index, one
//! `Arc<Mutex<postgres::Client>>` bridged onto its own OS thread (same
//! connect-and-init-schema-in-one-call shape as
//! [`crate::capability_graph::PgCapabilityGraph`] and `AgentTaskLedger`),
//! and tenant isolation via a `WHERE tenant_id = $n` clause rather than RLS
//! or a schema-per-tenant scheme.
//!
//! **Deliberate divergence from `agent_tasks`:** a task is never deleted;
//! an insight is, once resolved (shipped into a skill, or rejected) — see
//! ADR-013 §3 Stage 2's divergence note. Phase 1 never reaches a resolved
//! state (everything stays `new`), so deletion isn't exercised yet, but the
//! schema and `delete_insight` are here so Phase 3 doesn't need a migration
//! to add them.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use postgres::Client;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;
use uuid::Uuid;

/// What tripped the cheap gate for this row. Purely descriptive — nothing
/// branches on this beyond filtering/reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightSource {
    ToolFailure,
    Escalation,
}

impl InsightSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            InsightSource::ToolFailure => "tool_failure",
            InsightSource::Escalation => "escalation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tool_failure" => Some(InsightSource::ToolFailure),
            "escalation" => Some(InsightSource::Escalation),
            _ => None,
        }
    }
}

/// Lifecycle state of an insight row. `New` is the only state Phase 1
/// writes or reads — `Triaged`/`InProgress` are Phase 2/3 territory
/// (insight synthesis, kanban) and exist here only so those phases don't
/// need a schema change to introduce them (the column is plain `TEXT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightStatus {
    New,
    Triaged,
    InProgress,
}

impl InsightStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InsightStatus::New => "new",
            InsightStatus::Triaged => "triaged",
            InsightStatus::InProgress => "in_progress",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "new" => Some(InsightStatus::New),
            "triaged" => Some(InsightStatus::Triaged),
            "in_progress" => Some(InsightStatus::InProgress),
            _ => None,
        }
    }
}

/// One row of the ledger, as read back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInsight {
    pub insight_id: String,
    pub tenant_id: String,
    pub agent_type: String,
    pub session_id: Option<String>,
    pub source: InsightSource,
    pub signal: String,
    pub status: InsightStatus,
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
        .name("pg-skill-insight-op".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("failed to spawn pg skill insight ledger thread")?;
    rx.await.map_err(|_| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "pg skill insight ledger thread terminated unexpectedly"
        );
        anyhow::Error::msg("pg skill insight ledger thread terminated unexpectedly")
    })?
}

/// Drops its inner value on a background OS thread — see
/// `task_ledger::DropOnThread` (duplicated, not shared, same
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
            .name("pg-skill-insight-drop".to_string())
            .spawn(move || drop(std::mem::ManuallyDrop::into_inner(slot)))
            .is_err()
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "pg-skill-insight-drop thread spawn failed; leaking client to avoid nested-runtime panic"
            );
        }
    }
}

/// Postgres-backed skill insight ledger. Own connection, independent of
/// `PostgresMemory`/`AgentTaskLedger` — same one-extra-connection tradeoff
/// those make, well inside the tuned 200-connection budget.
pub struct AgentSkillInsightLedger {
    client: DropOnThread<Arc<Mutex<Client>>>,
    schema: String,
}

impl AgentSkillInsightLedger {
    /// Opens a new connection and ensures the table exists. `db_url` is the
    /// same libpq key=value DSN every other Cerveau Postgres consumer uses.
    pub async fn connect(db_url: &str, schema: &str) -> Result<Self> {
        let db_url = db_url.to_string();
        let schema_owned = schema.to_string();
        let client = run_on_os_thread(move || -> Result<Client> {
            let mut client =
                Client::connect(&db_url, postgres::NoTls).context("connect to Postgres")?;
            client.batch_execute(&format!(
                r#"
                CREATE TABLE IF NOT EXISTS "{schema_owned}".skill_insights (
                    insight_id TEXT PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    agent_type TEXT NOT NULL,
                    session_id TEXT,
                    source TEXT NOT NULL,
                    signal TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'new',
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );
                CREATE INDEX IF NOT EXISTS idx_skill_insights_lookup
                    ON "{schema_owned}".skill_insights(tenant_id, agent_type, status);
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

    /// Record a cheap-gate signal. Returns the generated `insight_id`.
    /// Always lands in `status = 'new'` — nothing in Phase 1 moves it
    /// forward.
    pub async fn create_insight(
        &self,
        tenant_id: &str,
        agent_type: &str,
        session_id: Option<&str>,
        source: InsightSource,
        signal: &str,
    ) -> Result<String> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let insight_id = Uuid::new_v4().to_string();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        let session_id = session_id.map(str::to_string);
        let source_str = source.as_str().to_string();
        let signal = signal.to_string();
        let id_out = insight_id.clone();
        run_on_os_thread(move || -> Result<()> {
            let mut client = client.lock();
            client.execute(
                &format!(
                    r#"INSERT INTO "{schema}".skill_insights
                           (insight_id, tenant_id, agent_type, session_id, source, signal, status)
                       VALUES ($1, $2, $3, $4, $5, $6, 'new')"#
                ),
                &[
                    &insight_id,
                    &tenant_id,
                    &agent_type,
                    &session_id,
                    &source_str,
                    &signal,
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(id_out)
    }

    /// List a tenant+agent-type's insights, optionally filtered to one
    /// status. Ordered newest-first. Read-only inspection for Phase 1 —
    /// there is no tool wrapper yet; query this directly (or via `psql`)
    /// while validating how often the cheap gate actually fires.
    pub async fn list_insights(
        &self,
        tenant_id: &str,
        agent_type: &str,
        status: Option<InsightStatus>,
    ) -> Result<Vec<SkillInsight>> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let agent_type = agent_type.to_string();
        let status_str = status.map(|s| s.as_str().to_string());
        run_on_os_thread(move || -> Result<Vec<SkillInsight>> {
            let mut client = client.lock();
            let rows = match &status_str {
                Some(s) => client.query(
                    &format!(
                        r#"SELECT insight_id, tenant_id, agent_type, session_id, source, signal,
                                  status, created_at, updated_at
                           FROM "{schema}".skill_insights
                           WHERE tenant_id = $1 AND agent_type = $2 AND status = $3
                           ORDER BY created_at DESC"#
                    ),
                    &[&tenant_id, &agent_type, s],
                )?,
                None => client.query(
                    &format!(
                        r#"SELECT insight_id, tenant_id, agent_type, session_id, source, signal,
                                  status, created_at, updated_at
                           FROM "{schema}".skill_insights
                           WHERE tenant_id = $1 AND agent_type = $2
                           ORDER BY created_at DESC"#
                    ),
                    &[&tenant_id, &agent_type],
                )?,
            };
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let source_raw: String = row.get(4);
                let status_raw: String = row.get(6);
                out.push(SkillInsight {
                    insight_id: row.get(0),
                    tenant_id: row.get(1),
                    agent_type: row.get(2),
                    session_id: row.get(3),
                    source: InsightSource::parse(&source_raw).unwrap_or(InsightSource::ToolFailure),
                    signal: row.get(5),
                    status: InsightStatus::parse(&status_raw).unwrap_or(InsightStatus::New),
                    created_at: row.get(7),
                    updated_at: row.get(8),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Delete a resolved insight. Unused in Phase 1 (nothing ever resolves
    /// yet) — present so Phase 3's "delete once shipped into a skill"
    /// lifecycle (ADR-013 §3 Stage 2/3) needs no schema/API change to land.
    pub async fn delete_insight(&self, tenant_id: &str, insight_id: &str) -> Result<bool> {
        let client = Arc::clone(self.client.get());
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let insight_id = insight_id.to_string();
        let rows = run_on_os_thread(move || -> Result<u64> {
            let mut client = client.lock();
            let rows = client.execute(
                &format!(
                    r#"DELETE FROM "{schema}".skill_insights WHERE insight_id = $1 AND tenant_id = $2"#
                ),
                &[&insight_id, &tenant_id],
            )?;
            Ok(rows)
        })
        .await?;
        Ok(rows > 0)
    }
}

// ── Process-wide install hook ────────────────────────────────────────────
//
// Same "install once at daemon startup, absent ⇒ None ⇒ the cheap-gate
// write site simply no-ops" shape as `task_ledger`'s singleton.
static SKILL_INSIGHT_LEDGER: OnceLock<Arc<AgentSkillInsightLedger>> = OnceLock::new();

/// Register the process-wide skill insight ledger. Idempotent — only the
/// first call takes effect.
pub fn install_skill_insight_ledger(ledger: Arc<AgentSkillInsightLedger>) {
    let _ = SKILL_INSIGHT_LEDGER.set(ledger);
}

/// The installed skill insight ledger, if any. `None` when
/// `[skill_insights].enabled = false` (the default), the active memory
/// backend isn't Postgres, or before daemon startup has run its
/// installation step (e.g. in unit tests).
pub fn current_skill_insight_ledger() -> Option<Arc<AgentSkillInsightLedger>> {
    SKILL_INSIGHT_LEDGER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_round_trips() {
        for s in [InsightSource::ToolFailure, InsightSource::Escalation] {
            assert_eq!(InsightSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(InsightSource::parse("bogus"), None);
    }

    #[test]
    fn status_round_trips() {
        for s in [
            InsightStatus::New,
            InsightStatus::Triaged,
            InsightStatus::InProgress,
        ] {
            assert_eq!(InsightStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(InsightStatus::parse("bogus"), None);
    }
}
