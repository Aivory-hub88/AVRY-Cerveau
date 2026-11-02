//! ADR-021 §4.3: quiet and failed unattended runs.
//!
//! `run_no_update` and `run_report_failure` let an agent on an unattended run
//! (a cron job, a reminder that fired, a wait that woke) say how the run
//! ended. A call records a [`RunOutcome`] into
//! [`crate::agent::tenant::RUN_OUTCOME`]; it has no side effect of its own.
//! After the turn the caller acts on it: the cron scheduler skips delivery for
//! a quiet run and records a failed one as `error` on `cron_runs`, and the
//! gateway returns `run_outcome` so the room posts nothing or marks the task
//! failed. So the tools are `Safe` tier and never park on an approval.
//!
//! They are only offered when the cell is scoped, never on a conversation
//! turn, where "send nothing" would leave a person without an answer.

use crate::agent::tenant::{RunOutcome, record_run_outcome};
use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

/// The two tool names, for registration and the risk-tier safe list.
pub const RUN_OUTCOME_TOOL_NAMES: [&str; 2] = ["run_no_update", "run_report_failure"];

const MAX_REASON_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    NoUpdate,
    ReportFailure,
}

/// One of the two run-outcome tools.
pub struct RunOutcomeTool {
    kind: RunKind,
}

impl RunOutcomeTool {
    /// Both tools, in [`RUN_OUTCOME_TOOL_NAMES`] order.
    pub fn all() -> Vec<Self> {
        [RunKind::NoUpdate, RunKind::ReportFailure]
            .into_iter()
            .map(|kind| Self { kind })
            .collect()
    }

    fn refuse(message: impl Into<String>) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some(message.into()),
        })
    }
}

#[async_trait]
impl Tool for RunOutcomeTool {
    fn name(&self) -> &str {
        match self.kind {
            RunKind::NoUpdate => "run_no_update",
            RunKind::ReportFailure => "run_report_failure",
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            RunKind::NoUpdate => {
                "This is a scheduled or unattended run. Call this when the run found nothing new worth \
                 telling anyone (no new leads, nothing changed since last time): no notification is \
                 sent and your reply is not posted. Do not call it when you have something to report."
            }
            RunKind::ReportFailure => {
                "This is a scheduled or unattended run. Call this when the run could not do its job \
                 (a source was unreachable, data was missing, a step was refused). The run is recorded \
                 as failed with your reason, and the reason is shown to the people who rely on it."
            }
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        match self.kind {
            RunKind::NoUpdate => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            RunKind::ReportFailure => json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "minLength": 1, "maxLength": MAX_REASON_CHARS,
                                "description": "What went wrong, in one or two sentences a person can act on." }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let outcome = match self.kind {
            RunKind::NoUpdate => RunOutcome::Quiet,
            RunKind::ReportFailure => {
                let reason = args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if reason.is_empty() {
                    return Self::refuse("'reason' must say what went wrong");
                }
                if reason.chars().count() > MAX_REASON_CHARS {
                    return Self::refuse(format!(
                        "'reason' is longer than {MAX_REASON_CHARS} characters"
                    ));
                }
                RunOutcome::Failed {
                    reason: reason.to_string(),
                }
            }
        };
        if let Err(e) = record_run_outcome(outcome) {
            return Self::refuse(e);
        }
        let note = match self.kind {
            RunKind::NoUpdate => {
                "Recorded: this run ends quietly, nothing is sent. End the turn with a one-line summary \
                 for the run log."
            }
            RunKind::ReportFailure => {
                "Recorded: this run is marked failed with your reason. End the turn with a short \
                 explanation of what failed."
            }
        };
        Ok(ToolResult {
            success: true,
            output: note.into(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tenant::{RUN_OUTCOME, take_run_outcome};
    use std::sync::Arc;

    fn tool(name: &str) -> RunOutcomeTool {
        RunOutcomeTool::all()
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap()
    }

    async fn unattended<F: std::future::Future>(f: F) -> (F::Output, Option<RunOutcome>) {
        RUN_OUTCOME
            .scope(Some(Arc::new(parking_lot::Mutex::new(None))), async {
                let out = f.await;
                (out, take_run_outcome())
            })
            .await
    }

    #[tokio::test]
    async fn no_update_records_a_quiet_run() {
        let (result, outcome) = unattended(tool("run_no_update").execute(json!({}))).await;
        assert!(result.unwrap().success);
        assert_eq!(outcome, Some(RunOutcome::Quiet));
    }

    #[tokio::test]
    async fn report_failure_records_the_reason() {
        let (result, outcome) = unattended(
            tool("run_report_failure").execute(json!({"reason": "  Odoo was unreachable  "})),
        )
        .await;
        assert!(result.unwrap().success);
        assert_eq!(
            outcome,
            Some(RunOutcome::Failed {
                reason: "Odoo was unreachable".into()
            })
        );
        assert_eq!(outcome.as_ref().map(RunOutcome::as_str), Some("failed"));
    }

    #[tokio::test]
    async fn a_failure_after_a_quiet_call_wins() {
        let (_, outcome) = unattended(async {
            tool("run_no_update").execute(json!({})).await.unwrap();
            tool("run_report_failure")
                .execute(json!({"reason": "the CRM export was empty"}))
                .await
                .unwrap();
        })
        .await;
        assert_eq!(
            outcome.as_ref().and_then(RunOutcome::reason),
            Some("the CRM export was empty")
        );
    }

    #[tokio::test]
    async fn an_empty_reason_is_refused() {
        let (result, outcome) =
            unattended(tool("run_report_failure").execute(json!({"reason": " "}))).await;
        assert!(!result.unwrap().success);
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn outside_an_unattended_run_the_tools_refuse() {
        let result = tool("run_no_update").execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unattended run"));
        assert_eq!(take_run_outcome(), None);
    }
}
