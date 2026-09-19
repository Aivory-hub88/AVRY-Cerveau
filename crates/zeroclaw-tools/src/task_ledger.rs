//! Aivory Cerveau: Agent Task Ledger — `task_create` / `task_update_status`
//! / `task_list`. Companion to `graph_remember`/`graph_recall`
//! (`graph_memory.rs`), not a replacement: those hit the cognee-rs sidecar
//! for durable semantic facts. This hits a plain Postgres row store for
//! task *lifecycle* — what the agent is doing right now, what's blocked on
//! a human, what's done — a question a row scan answers deterministically
//! where a semantic search can only answer best-effort.
//!
//! **Tenant-only, deliberately, and enforced at construction time** — same
//! posture as `graph_memory.rs`. `zeroclaw-tools` cannot see
//! `zeroclaw-runtime::agent::tenant` (the dependency runs the other way),
//! so tenant identity is resolved once by the caller
//! (`zeroclaw-runtime`'s tool-registry construction) and handed to `new()`.
//! The wiring caller only constructs these tools at all when a tenant
//! context and an installed ledger both exist — see `all_tools_with_runtime`
//! — so by the time any of these `execute()`s run, both are guaranteed
//! present.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::task_ledger::{AgentTaskLedger, StatusUpdate, TaskPriority, TaskStatus};

/// Shared tenant addressing every task-ledger tool needs. Resolved once by
/// the caller — see the module doc for why this crate can't resolve it
/// itself.
#[derive(Clone)]
struct TaskLedgerContext {
    ledger: Arc<AgentTaskLedger>,
    tenant_id: String,
    agent_type: String,
    session_id: Option<String>,
}

fn priority_of(args: &serde_json::Value) -> TaskPriority {
    match args.get("priority").and_then(|v| v.as_str()) {
        Some(s) => TaskPriority::parse(s).unwrap_or(TaskPriority::Normal),
        None => TaskPriority::Normal,
    }
}

/// Start tracking a new task, in `todo`. Use for anything worth resuming
/// across a session boundary -- a multi-step job, something waiting on a
/// later action, or a fact worth surfacing to the operator once it's
/// blocked or done. Not for trivial one-shot replies that finish within
/// the same turn; that's what the ledger deliberately doesn't track.
pub struct TaskCreateTool {
    ctx: TaskLedgerContext,
}

impl TaskCreateTool {
    pub fn new(
        ledger: Arc<AgentTaskLedger>,
        tenant_id: String,
        agent_type: String,
        session_id: Option<String>,
    ) -> Self {
        Self {
            ctx: TaskLedgerContext {
                ledger,
                tenant_id,
                agent_type,
                session_id,
            },
        }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }

    fn description(&self) -> &str {
        "Start tracking a new task on your own to-do ledger, in status 'todo'. Use this for \
         anything worth resuming if this session ends -- a multi-step job, something you're \
         about to wait on, or work you may need to report back on later. Returns a task_id; \
         keep it to update the task's status as you make progress."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short, specific description of the task (shown as-is to the operator if it ever becomes blocked or done)."
                },
                "priority": {
                    "type": "string",
                    "enum": ["low", "normal", "high"],
                    "description": "Defaults to 'normal'."
                }
            },
            "required": ["title"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Orphan sweep first: park this tenant+agent's dead in-progress rows
        // from other sessions before doing anything else. Never fails the
        // call — a sweep error is logged, the requested operation proceeds.
        if let Err(e) = self
            .ctx
            .ledger
            .park_orphaned_tasks(
                &self.ctx.tenant_id,
                &self.ctx.agent_type,
                self.ctx.session_id.as_deref(),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                "orphan sweep failed; continuing with the requested ledger operation"
            );
        }
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'title' must be non-empty".to_string()),
            });
        }
        let priority = priority_of(&args);

        match self
            .ctx
            .ledger
            .create_task(
                &self.ctx.tenant_id,
                &self.ctx.agent_type,
                self.ctx.session_id.as_deref(),
                title,
                priority,
            )
            .await
        {
            Ok(task_id) => Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    json!({"task_id": task_id, "status": "todo"}),
                    format!("Created task {task_id} (todo): {title}"),
                ),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to create task: {e}")),
            }),
        }
    }
}

/// Move a tracked task to a new status. The only way a task's state
/// changes -- there is no delete; a task reaching 'done' just stops
/// showing up in a `todo`/`in_progress`/`blocked` listing.
///
/// **Notification, on transition into `blocked`/`done` only.** Posts to
/// avry-backend's existing `/api/v1/agent-actions/internal` (the same
/// endpoint `lead`/`ticket`/`invoice`/... already go through, action_type
/// `"task"`) so the operator sees it in the dashboard's Agent Activity feed
/// without reading a transcript. `notify` is the avry-backend
/// `(base_url, token)` seam, resolved once by the caller the same way
/// tenant identity is (see the module doc) -- `zeroclaw-tools` cannot read
/// `zeroclaw_runtime::cron::tenant_sync::backend()` itself. `None` when the
/// seam isn't configured on this host -- the tool still works, it just
/// stays local like it always has. Fire-and-forget in spirit even though
/// it's awaited inline: any failure (unreachable backend, non-2xx) is
/// logged and swallowed, never turns a successful status update into a
/// failed tool call.
pub struct TaskUpdateStatusTool {
    ctx: TaskLedgerContext,
    notify: Option<(String, String)>,
}

impl TaskUpdateStatusTool {
    pub fn new(
        ledger: Arc<AgentTaskLedger>,
        tenant_id: String,
        agent_type: String,
        session_id: Option<String>,
        notify: Option<(String, String)>,
    ) -> Self {
        Self {
            ctx: TaskLedgerContext {
                ledger,
                tenant_id,
                agent_type,
                session_id,
            },
            notify,
        }
    }
}

fn notify_client() -> reqwest::Client {
    zeroclaw_config::schema::build_runtime_proxy_client_with_timeouts(
        "tool.task_update_status",
        15,
        5,
    )
}

/// Best-effort report of one blocked/done transition to avry-backend's
/// Agent Activity feed. `task` is the freshly-updated row (fetched by the
/// caller right after the write, so the payload reflects what was actually
/// stored, not just the raw tool args). Never returns an error -- see the
/// struct doc for why a notify failure must not surface as a tool failure.
/// Only a write that really changed a blocked/done state is worth telling
/// avry-backend about: a no-op (operator-stopped task, `done` on an already-done
/// task) would post a phantom `task` action -- for a stopped task, one whose
/// status reads `cancelled`.
fn should_notify(status: TaskStatus, outcome: StatusUpdate) -> bool {
    outcome == StatusUpdate::Applied && matches!(status, TaskStatus::Blocked | TaskStatus::Done)
}

/// What the agent is told after a status write, given what the ledger says
/// actually happened. "Task X is now done" is only ever said when it is true:
/// a stopped task ignores late writes by design (never a resurrection), and
/// reporting success for that made the agent believe finished work was
/// delivered. The stopped case is a refusal (`success: false`), like the other
/// terminal-state refusals, and tells the agent not to retry.
fn update_outcome_result(task_id: &str, status: TaskStatus, outcome: StatusUpdate) -> ToolResult {
    match outcome {
        StatusUpdate::Applied => ToolResult {
            success: true,
            output: ToolOutput::text(format!("Task {task_id} is now {}", status.as_str())),
            error: None,
        },
        StatusUpdate::AlreadyDone => ToolResult {
            success: true,
            output: ToolOutput::text(format!("Task {task_id} was already done; nothing changed.")),
            error: None,
        },
        StatusUpdate::StoppedByOperator => ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some(format!(
                "NOT APPLIED: task {task_id} was stopped by an operator (Mission Control Stop) and \
                 stays stopped, so setting it to '{}' had no effect. Do not retry, re-open or \
                 re-create it; if the work is still wanted, ask the user.",
                status.as_str()
            )),
        },
    }
}

async fn notify_agent_action(
    base_url: &str,
    token: &str,
    ctx: &TaskLedgerContext,
    task: &zeroclaw_memory::task_ledger::AgentTask,
) {
    post_task_action(
        base_url,
        token,
        &ctx.tenant_id,
        &ctx.agent_type,
        ctx.session_id.as_deref(),
        task,
    )
    .await;
}

/// Whether a task moving to `status` is worth an entry in the operator's Agent
/// Activity feed: only a transition into `blocked` (needs a person) or `done`.
pub fn transition_notifies(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Blocked | TaskStatus::Done)
}

/// Post a task transition made by something other than the agent's own
/// `task_update_status` -- the delegation engine settling a delegated task, or the
/// reaper's reconciler. Those writes used to be invisible to the notification feed,
/// so a delegation that failed or finished showed on the board but nowhere else.
/// Addressed by the row itself (its tenant, agent_type and session), because there
/// is no calling turn to take them from. Failures are logged and swallowed, like
/// the tool's own notify.
pub async fn notify_task_transition(
    base_url: &str,
    token: &str,
    task: &zeroclaw_memory::task_ledger::AgentTask,
) {
    if !transition_notifies(task.status) {
        return;
    }
    post_task_action(
        base_url,
        token,
        &task.tenant_id,
        &task.agent_type,
        task.session_id.as_deref(),
        task,
    )
    .await;
}

async fn post_task_action(
    base_url: &str,
    token: &str,
    tenant_id: &str,
    agent_type: &str,
    session_id: Option<&str>,
    task: &zeroclaw_memory::task_ledger::AgentTask,
) {
    let payload = json!({
        "task_id": task.task_id,
        "title": task.title,
        "status": task.status.as_str(),
        "blocked_reason": task.blocked_reason,
    });
    let body = json!({
        "user_id": tenant_id,
        "agent_type": agent_type,
        "action_type": "task",
        "payload": payload,
        "session_id": session_id,
    });
    let res = notify_client()
        .post(format!("{base_url}/api/v1/agent-actions/internal"))
        .header("X-Internal-Token", token)
        .json(&body)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_attrs(::serde_json::json!({ "status": r.status().as_u16() })),
                "task_update_status: agent-actions notify rejected by avry-backend"
            );
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_attrs(::serde_json::json!({ "error": e.to_string() })),
                "task_update_status: could not reach avry-backend to notify agent-actions"
            );
        }
    }
}

#[async_trait]
impl Tool for TaskUpdateStatusTool {
    fn name(&self) -> &str {
        "task_update_status"
    }

    fn description(&self) -> &str {
        "Move a task you created (via task_create) to a new status: 'todo', 'in_progress', \
         'blocked', or 'done'. When setting 'blocked', you must give a specific reason -- it's \
         what the operator sees, so write it for them, not for yourself (e.g. 'waiting on \
         approval to send this invoice', not 'stuck'). A task moved to 'done' is terminal -- if \
         you need to correct something afterward, create a new task referencing this one \
         instead of trying to reopen it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task_id returned by task_create."
                },
                "status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "blocked", "done"]
                },
                "blocked_reason": {
                    "type": "string",
                    "description": "Required when status is 'blocked'. A specific, operator-facing reason."
                }
            },
            "required": ["task_id", "status"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Orphan sweep first (see TaskCreateTool): park dead in_progress
        // rows from other sessions. Never fails the call.
        if let Err(e) = self
            .ctx
            .ledger
            .park_orphaned_tasks(
                &self.ctx.tenant_id,
                &self.ctx.agent_type,
                self.ctx.session_id.as_deref(),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                "orphan sweep failed; continuing with the requested ledger operation"
            );
        }
        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if task_id.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'task_id' must be non-empty".to_string()),
            });
        }

        let status_raw = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let Some(status) = TaskStatus::parse(status_raw) else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "'status' must be one of todo, in_progress, blocked, done -- got '{status_raw}'"
                )),
            });
        };

        let blocked_reason = args
            .get("blocked_reason")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if status == TaskStatus::Cancelled {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    "only the operator stops tasks (Mission Control Stop button) — \
                     mark 'blocked' with a reason instead"
                        .to_string(),
                ),
            });
        }

        if status == TaskStatus::Blocked && blocked_reason.is_none() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'blocked_reason' is required when status is 'blocked'".to_string()),
            });
        }

        match self
            .ctx
            .ledger
            .update_status(&self.ctx.tenant_id, task_id, status, blocked_reason)
            .await
        {
            Ok(outcome) => {
                if should_notify(status, outcome) {
                    if let Some((base_url, token)) = &self.notify {
                        if let Ok(Some(task)) =
                            self.ctx.ledger.get_task(&self.ctx.tenant_id, task_id).await
                        {
                            notify_agent_action(base_url, token, &self.ctx, &task).await;
                        }
                    }
                }
                Ok(update_outcome_result(task_id, status, outcome))
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to update task: {e}")),
            }),
        }
    }
}

/// List your own tracked tasks, optionally filtered to one status. Call
/// this with no filter at the start of a new session -- it's the
/// deterministic answer to "what was I in the middle of", cheaper and
/// exact where memory recall can only be best-effort.
pub struct TaskListTool {
    ctx: TaskLedgerContext,
}

impl TaskListTool {
    pub fn new(
        ledger: Arc<AgentTaskLedger>,
        tenant_id: String,
        agent_type: String,
        session_id: Option<String>,
    ) -> Self {
        Self {
            ctx: TaskLedgerContext {
                ledger,
                tenant_id,
                agent_type,
                session_id,
            },
        }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        "List your own tracked tasks (from task_create), optionally filtered to one status. \
         Call this with no filter at the start of a new session to recall what you were in the \
         middle of -- it's a row scan of exact state, not a best-effort semantic search. \
         Set scope=\"session\" to list EVERY agent's tasks in this conversation (yours and the \
         specialists' you delegated to, with who owns each): that is how you check whether \
         delegated work is finished. By default (scope=\"mine\") you only ever see your own."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "blocked", "done"],
                    "description": "Omit to list every task regardless of status."
                },
                "scope": {
                    "type": "string",
                    "enum": ["mine", "session"],
                    "default": "mine",
                    "description": "mine = only your own tasks (default). session = every agent's tasks in this conversation session, each labelled with its owner."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Orphan sweep first (see TaskCreateTool): park dead in_progress
        // rows from other sessions. Never fails the call.
        if let Err(e) = self
            .ctx
            .ledger
            .park_orphaned_tasks(
                &self.ctx.tenant_id,
                &self.ctx.agent_type,
                self.ctx.session_id.as_deref(),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                "orphan sweep failed; continuing with the requested ledger operation"
            );
        }
        let status = match args.get("status").and_then(|v| v.as_str()) {
            Some(s) => match TaskStatus::parse(s) {
                Some(status) => Some(status),
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "'status' must be one of todo, in_progress, blocked, done -- got '{s}'"
                        )),
                    });
                }
            },
            None => None,
        };

        let session_scope = match args.get("scope").and_then(|v| v.as_str()) {
            None | Some("mine") => false,
            Some("session") => true,
            Some(other) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "'scope' must be \"mine\" or \"session\" -- got '{other}'"
                    )),
                });
            }
        };

        let fetched = if session_scope {
            let Some(session_id) = self.ctx.session_id.as_deref() else {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(
                        "scope=\"session\" needs a conversation session, and this turn has none. \
                         Use scope=\"mine\" (your own tasks) instead."
                            .to_string(),
                    ),
                });
            };
            self.ctx
                .ledger
                .list_session_tasks(&self.ctx.tenant_id, session_id, status)
                .await
        } else {
            self.ctx
                .ledger
                .list_tasks(&self.ctx.tenant_id, &self.ctx.agent_type, status)
                .await
        };

        match fetched {
            Ok(tasks) => {
                if tasks.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: ToolOutput::json_with_text(
                            json!({"tasks": []}),
                            if session_scope {
                                "No tracked tasks in this session."
                            } else {
                                "No tracked tasks."
                            },
                        ),
                        error: None,
                    });
                }
                let data = json!({
                    "scope": if session_scope { "session" } else { "mine" },
                    "tasks": tasks.iter().map(|t| json!({
                        "task_id": t.task_id,
                        "agent_type": t.agent_type,
                        "title": t.title,
                        "status": t.status.as_str(),
                        "priority": t.priority.as_str(),
                        "blocked_reason": t.blocked_reason,
                        "updated_at": t.updated_at.to_rfc3339(),
                    })).collect::<Vec<_>>()
                });
                let mut text = String::new();
                for t in &tasks {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    // The owner matters only when the list spans agents; for
                    // scope=mine the line keeps its long-standing shape.
                    let owner = if session_scope {
                        format!(" ({})", t.agent_type)
                    } else {
                        String::new()
                    };
                    text.push_str(&format!(
                        "[{}]{} {} ({}) -- {}",
                        t.status.as_str(),
                        owner,
                        t.title,
                        t.task_id,
                        t.blocked_reason.as_deref().unwrap_or("")
                    ));
                }
                Ok(ToolResult {
                    success: true,
                    output: ToolOutput::json_with_text(data, text),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to list tasks: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod update_outcome_tests {
    use super::*;

    #[test]
    fn applied_write_says_the_new_status() {
        let r = update_outcome_result("t-1", TaskStatus::InProgress, StatusUpdate::Applied);
        assert!(r.success);
        assert_eq!(r.output.to_string(), "Task t-1 is now in_progress");
    }

    #[test]
    fn stopped_task_is_reported_as_not_applied_never_as_now_done() {
        let r = update_outcome_result("t-1", TaskStatus::Done, StatusUpdate::StoppedByOperator);
        assert!(!r.success, "a refused write is not a success");
        let error = r.error.expect("refusal carries the explanation");
        assert!(error.contains("NOT APPLIED"), "{error}");
        assert!(error.contains("stopped by an operator"), "{error}");
        assert!(error.contains("Do not retry"), "{error}");
        assert!(
            !error.contains("is now"),
            "must never claim the new status: {error}"
        );
        assert!(
            r.output.to_string().is_empty(),
            "no success text alongside a refusal"
        );
    }

    #[test]
    fn done_twice_says_nothing_changed() {
        let r = update_outcome_result("t-1", TaskStatus::Done, StatusUpdate::AlreadyDone);
        assert!(r.success);
        let text = r.output.to_string();
        assert!(
            text.contains("already done") && text.contains("nothing changed"),
            "{text}"
        );
        assert!(!text.contains("is now"), "{text}");
    }

    #[test]
    fn only_a_real_blocked_or_done_change_notifies_avry_backend() {
        use StatusUpdate::{AlreadyDone, Applied, StoppedByOperator};
        assert!(should_notify(TaskStatus::Done, Applied));
        assert!(should_notify(TaskStatus::Blocked, Applied));
        assert!(
            !should_notify(TaskStatus::InProgress, Applied),
            "not a notifiable state"
        );
        assert!(
            !should_notify(TaskStatus::Done, AlreadyDone),
            "no state change"
        );
        assert!(
            !should_notify(TaskStatus::Done, StoppedByOperator),
            "phantom cancelled action"
        );
        assert!(!should_notify(TaskStatus::Blocked, StoppedByOperator));
    }
}

/// `task_list` as the model sees it, against a real Postgres. Needs
/// `CERVEAU_TEST_PG_URL` (a no-op otherwise); every row is keyed by a fresh tenant
/// so runs and other tests cannot see each other.
#[cfg(test)]
mod task_list_scope_tests {
    use super::*;

    /// One ledger for every test in this module: `connect` runs DDL, and several
    /// tests connecting to the same schema at once would race on it (the daemon
    /// connects once, at start-up).
    async fn ledger() -> Option<Arc<AgentTaskLedger>> {
        static LEDGER: tokio::sync::OnceCell<Option<Arc<AgentTaskLedger>>> =
            tokio::sync::OnceCell::const_new();
        LEDGER
            .get_or_init(|| async {
                let url = std::env::var("CERVEAU_TEST_PG_URL")
                    .ok()
                    .filter(|s| !s.is_empty())?;
                Some(Arc::new(
                    AgentTaskLedger::connect(&url, "public")
                        .await
                        .expect("connect"),
                ))
            })
            .await
            .clone()
    }

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    async fn list(tool: &TaskListTool, args: serde_json::Value) -> ToolResult {
        tool.execute(args)
            .await
            .expect("task_list never errors out of band")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_scope_lets_an_orchestrator_see_what_it_delegated() {
        let Some(ledger) = ledger().await else {
            eprintln!("CERVEAU_TEST_PG_URL unset — skipping task_list scope test");
            return;
        };
        let tenant = unique("tenant");
        let session = unique("session");
        ledger
            .create_task(
                &tenant,
                "chief_of_staff",
                Some(&session),
                "AIRA-orch: qualify leads",
                TaskPriority::High,
            )
            .await
            .unwrap();
        let child = ledger
            .create_task(
                &tenant,
                "leads_qualifier",
                Some(&session),
                "qualify the new leads",
                TaskPriority::Normal,
            )
            .await
            .unwrap();
        ledger
            .update_status(&tenant, &child, TaskStatus::Done, None)
            .await
            .unwrap();

        let aira = TaskListTool::new(
            Arc::clone(&ledger),
            tenant.clone(),
            "chief_of_staff".into(),
            Some(session.clone()),
        );

        // The real schema advertises the parameter, defaulting to the safe scope.
        let schema = aira.parameters_schema();
        assert_eq!(schema["properties"]["scope"]["default"], "mine");
        assert_eq!(
            schema["properties"]["scope"]["enum"],
            json!(["mine", "session"])
        );

        // Default scope is unchanged: only her own rows, in the long-standing shape.
        let mine = list(&aira, json!({})).await;
        assert!(mine.success, "{mine:?}");
        let text = mine.output.to_string();
        assert!(text.contains("AIRA-orch: qualify leads"));
        assert!(
            !text.contains("qualify the new leads"),
            "scope=mine must not show a specialist's row: {text}"
        );
        assert!(
            text.starts_with("[todo] AIRA-orch"),
            "mine keeps its old line shape: {text}"
        );

        // scope=session shows the specialist's finished child, labelled with its owner.
        let all = list(&aira, json!({"scope": "session"})).await;
        assert!(all.success, "{all:?}");
        let text = all.output.to_string();
        assert!(
            text.contains("[done] (leads_qualifier) qualify the new leads"),
            "{text}"
        );
        assert!(
            text.contains("[todo] (chief_of_staff) AIRA-orch: qualify leads"),
            "{text}"
        );
        let data = all.output.data().expect("structured data");
        assert_eq!(data["scope"], "session");
        assert!(
            data["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["agent_type"] == "leads_qualifier")
        );

        // The status filter composes with the scope.
        let done = list(&aira, json!({"scope": "session", "status": "done"})).await;
        let text = done.output.to_string();
        assert!(
            text.contains("qualify the new leads") && !text.contains("AIRA-orch"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_scope_never_crosses_tenants_or_sessions() {
        let Some(ledger) = ledger().await else { return };
        let (tenant, other_tenant) = (unique("tenant"), unique("tenant"));
        let session = unique("session");
        ledger
            .create_task(
                &other_tenant,
                "chief_of_staff",
                Some(&session),
                "SECRET other tenant",
                TaskPriority::Normal,
            )
            .await
            .unwrap();
        ledger
            .create_task(
                &tenant,
                "chief_of_staff",
                Some("a-different-session"),
                "other session",
                TaskPriority::Normal,
            )
            .await
            .unwrap();
        ledger
            .create_task(
                &tenant,
                "chief_of_staff",
                Some(&session),
                "mine in this session",
                TaskPriority::Normal,
            )
            .await
            .unwrap();
        let tool = TaskListTool::new(
            Arc::clone(&ledger),
            tenant,
            "chief_of_staff".into(),
            Some(session),
        );
        let text = list(&tool, json!({"scope": "session"}))
            .await
            .output
            .to_string();
        assert!(text.contains("mine in this session"));
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("other session"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_scope_without_a_session_and_bad_scopes_explain_themselves() {
        let Some(ledger) = ledger().await else { return };
        let no_session = TaskListTool::new(
            Arc::clone(&ledger),
            unique("t"),
            "chief_of_staff".into(),
            None,
        );
        let r = list(&no_session, json!({"scope": "session"})).await;
        assert!(!r.success);
        let error = r.error.unwrap();
        assert!(
            error.contains("needs a conversation session") && error.contains("scope=\"mine\""),
            "{error}"
        );
        // The default still works without a session.
        assert!(list(&no_session, json!({})).await.success);

        let bad = list(&no_session, json!({"scope": "everyone"})).await;
        assert!(!bad.success);
        assert!(bad.error.unwrap().contains("'scope' must be"));
    }
}

#[cfg(test)]
mod notify_transition_tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_memory::task_ledger::AgentTask;

    fn task(status: TaskStatus, reason: Option<&str>) -> AgentTask {
        let now = chrono::Utc::now();
        AgentTask {
            task_id: "task-1".into(),
            tenant_id: "u1".into(),
            agent_type: "leads_qualifier".into(),
            session_id: Some("room-1".into()),
            title: "Delegated to leads_qualifier: qualify".into(),
            status,
            priority: TaskPriority::Normal,
            blocked_reason: reason.map(str::to_string),
            created_at: now,
            updated_at: now,
            context_id: None,
            parent_task_id: None,
            delegated_by: Some("chief_of_staff".into()),
            delegation_id: Some("d-1".into()),
            outcome: None,
            result_summary: None,
        }
    }

    #[test]
    fn only_blocked_and_done_are_worth_announcing() {
        assert!(transition_notifies(TaskStatus::Blocked));
        assert!(transition_notifies(TaskStatus::Done));
        for quiet in [
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::Cancelled,
        ] {
            assert!(!transition_notifies(quiet), "{quiet:?}");
        }
    }

    #[tokio::test]
    async fn a_settled_delegation_is_posted_addressed_by_its_own_row() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/agent-actions/internal"))
            .and(header("X-Internal-Token", "tok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        notify_task_transition(
            &server.uri(),
            "tok",
            &task(TaskStatus::Blocked, Some("Delegation failed: boom")),
        )
        .await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        // Addressed by the row, not by a calling turn (there is none).
        assert_eq!(body["user_id"], "u1");
        assert_eq!(body["agent_type"], "leads_qualifier");
        assert_eq!(body["action_type"], "task");
        assert_eq!(body["session_id"], "room-1");
        assert_eq!(body["payload"]["status"], "blocked");
        assert_eq!(body["payload"]["blocked_reason"], "Delegation failed: boom");
        assert_eq!(body["payload"]["task_id"], "task-1");
    }

    #[tokio::test]
    async fn quiet_statuses_and_an_unreachable_backend_are_harmless() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        for quiet in [
            TaskStatus::InProgress,
            TaskStatus::Cancelled,
            TaskStatus::Todo,
        ] {
            notify_task_transition(&server.uri(), "tok", &task(quiet, None)).await;
        }
        // A backend that is down, and one that rejects, are logged and swallowed.
        notify_task_transition("http://127.0.0.1:1", "tok", &task(TaskStatus::Done, None)).await;
        let rejecting = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&rejecting)
            .await;
        notify_task_transition(&rejecting.uri(), "tok", &task(TaskStatus::Done, None)).await;
    }
}
