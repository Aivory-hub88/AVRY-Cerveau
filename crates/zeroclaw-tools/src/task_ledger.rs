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
use zeroclaw_memory::task_ledger::{AgentTaskLedger, TaskPriority, TaskStatus};

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
        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
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
async fn notify_agent_action(
    base_url: &str,
    token: &str,
    ctx: &TaskLedgerContext,
    task: &zeroclaw_memory::task_ledger::AgentTask,
) {
    let payload = json!({
        "task_id": task.task_id,
        "title": task.title,
        "status": task.status.as_str(),
        "blocked_reason": task.blocked_reason,
    });
    let body = json!({
        "user_id": ctx.tenant_id,
        "agent_type": ctx.agent_type,
        "action_type": "task",
        "payload": payload,
        "session_id": ctx.session_id,
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
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("").trim();
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

        if status == TaskStatus::Blocked && blocked_reason.is_none() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    "'blocked_reason' is required when status is 'blocked'".to_string(),
                ),
            });
        }

        match self
            .ctx
            .ledger
            .update_status(&self.ctx.tenant_id, task_id, status, blocked_reason)
            .await
        {
            Ok(()) => {
                if matches!(status, TaskStatus::Blocked | TaskStatus::Done) {
                    if let Some((base_url, token)) = &self.notify {
                        if let Ok(Some(task)) =
                            self.ctx.ledger.get_task(&self.ctx.tenant_id, task_id).await
                        {
                            notify_agent_action(base_url, token, &self.ctx, &task).await;
                        }
                    }
                }
                Ok(ToolResult {
                    success: true,
                    output: ToolOutput::text(format!(
                        "Task {task_id} is now {}",
                        status.as_str()
                    )),
                    error: None,
                })
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
         middle of -- it's a row scan of exact state, not a best-effort semantic search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "blocked", "done"],
                    "description": "Omit to list every task regardless of status."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
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

        match self
            .ctx
            .ledger
            .list_tasks(&self.ctx.tenant_id, &self.ctx.agent_type, status)
            .await
        {
            Ok(tasks) => {
                if tasks.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: ToolOutput::json_with_text(
                            json!({"tasks": []}),
                            "No tracked tasks.",
                        ),
                        error: None,
                    });
                }
                let data = json!({
                    "tasks": tasks.iter().map(|t| json!({
                        "task_id": t.task_id,
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
                    text.push_str(&format!(
                        "[{}] {} ({}) -- {}",
                        t.status.as_str(),
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
