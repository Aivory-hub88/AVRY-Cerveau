//! ADR-014 Phase A2 — the delegation engine's link to the Agent Task Ledger.
//!
//! A tenant watches `cerveau.agent_tasks` in Mission Control. Until now nothing
//! tied a row there to the delegation doing its work, so Stop could not reach
//! the work, the 30-minute orphan sweep could bury a healthy long-running
//! delegation, and a failed delegation had no honest place to appear.
//!
//! The row is a *projection* of the delegation. The result file and the
//! `TaskRegistry` own execution state; this module mirrors it for humans and
//! never the other way round, so a ledger outage can never fail a delegation
//! (every call here is best-effort and logs instead of returning an error).
//!
//! Three moments, all opt-in on a live tenant turn with an installed ledger:
//! * a **background** delegation gets its row when it starts (or adopts one
//!   the caller already created, via `ledger_task_id`) and is settled when it
//!   ends;
//! * a **sync** hop gets a row only if it ends badly (`failed`) — a completed
//!   one finished inside a single turn and would only add two Postgres writes;
//! * a **reconciler** settles rows whose delegation ended without the engine
//!   writing it (a daemon restart), reading the registry.
//!
//! Built only with `memory-postgres`; without it every entry point is a no-op
//! so `delegate.rs` needs no `cfg` of its own.

#[cfg(feature = "memory-postgres")]
pub(crate) use real::*;
#[cfg(not(feature = "memory-postgres"))]
pub(crate) use stub::*;

/// What starting a background delegation's ledger row came to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "memory-postgres"), allow(dead_code))]
pub(crate) enum LedgerStart {
    /// No ledger, no tenant, or the ledger was unreachable: run unlinked.
    Unlinked,
    /// The row now tracking this delegation.
    Linked { ledger_task_id: String },
    /// `ledger_task_id` cannot be used; nothing should be started.
    Refused(String),
}

#[cfg(feature = "memory-postgres")]
mod real {
    use super::LedgerStart;
    use crate::agent::tenant::{current_tenant, current_turn_origin};
    use crate::control_plane::{TaskRegistry, TaskStatus};
    use crate::tools::delegate::BackgroundDelegateResult;
    use crate::tools::delegate::BackgroundTaskStatus;
    use crate::tools::delegate_envelope::{self as envelope, DelegateReason};
    use std::sync::Arc;
    use std::time::Duration;
    use zeroclaw_memory::task_ledger::{
        AdoptResult, AgentTaskLedger, DelegationEnd, NewDelegatedTask, TaskOutcome,
        TaskStatus as LedgerStatus, current_task_ledger,
    };

    /// How often a running background delegation looks at its own row to see
    /// whether the operator pressed Stop. One indexed read per delegation.
    const CANCEL_POLL: Duration = Duration::from_secs(5);
    const TITLE_PROMPT_CHARS: usize = 80;
    const REASON_CHARS: usize = 300;

    /// The tenant, session and ledger a delegation is recorded under, captured
    /// on the caller's task before anything is spawned (a spawned task does not
    /// inherit task-locals).
    #[derive(Clone)]
    pub(crate) struct LedgerLink {
        ledger: Arc<AgentTaskLedger>,
        tenant_id: String,
        /// The specialist doing the work; the dashboard matches rows by exact
        /// `agent_type`, so a product-agent target is relabelled like every
        /// other delegate-scoped identity.
        agent_type: String,
        delegated_by: String,
        session_id: Option<String>,
        /// avry-backend's `(base_url, token)` for the operator's notification
        /// feed, when this host has one configured.
        notify: Option<(String, String)>,
    }

    impl LedgerLink {
        /// A link with every field given, for tests that must control the
        /// notification target (the real one reads process environment).
        #[cfg(test)]
        pub(crate) fn for_test(
            ledger: Arc<AgentTaskLedger>,
            tenant_id: &str,
            agent_type: &str,
            delegated_by: &str,
            session_id: Option<&str>,
            notify: Option<(String, String)>,
        ) -> Self {
            Self {
                ledger,
                tenant_id: tenant_id.to_string(),
                agent_type: agent_type.to_string(),
                delegated_by: delegated_by.to_string(),
                session_id: session_id.map(str::to_string),
                notify,
            }
        }

        /// `None` when there is no installed ledger or no tenant context (host
        /// and internal turns have none), which is the common non-tenant case.
        pub(crate) fn capture(target: &str) -> Option<Self> {
            let ledger = current_task_ledger()?;
            let parent = current_tenant()?;
            let delegated_by = parent.agent_type.clone();
            let overlay = crate::tools::delegate::delegate_tenant_overlay(parent, target);
            Some(Self {
                ledger,
                tenant_id: overlay.platform_user_id.clone(),
                agent_type: overlay.agent_type.clone(),
                delegated_by,
                session_id: current_turn_origin().and_then(|o| o.session_id.clone()),
                notify: crate::cron::tenant_sync::backend(),
            })
        }

        /// Create the row for a background delegation, or adopt `adopt` (the
        /// caller's own row) instead. Best-effort: a ledger error becomes
        /// [`LedgerStart::Unlinked`], never a failed delegation.
        pub(crate) async fn start_background(
            &self,
            delegation_id: &str,
            agent: &str,
            prompt: &str,
            adopt: Option<&str>,
        ) -> LedgerStart {
            if let Some(task_id) = adopt {
                return match self
                    .ledger
                    .adopt_for_delegation(
                        &self.tenant_id,
                        task_id,
                        delegation_id,
                        &self.delegated_by,
                        None,
                    )
                    .await
                {
                    Ok(AdoptResult::Adopted) => LedgerStart::Linked {
                        ledger_task_id: task_id.to_string(),
                    },
                    Ok(other) => LedgerStart::Refused(adopt_refusal(task_id, other)),
                    Err(e) => {
                        warn("adopt", &e);
                        LedgerStart::Unlinked
                    }
                };
            }
            let title = title_for(agent, prompt);
            match self
                .ledger
                .create_delegated_task(NewDelegatedTask {
                    tenant_id: &self.tenant_id,
                    agent_type: &self.agent_type,
                    session_id: self.session_id.as_deref(),
                    title: &title,
                    delegated_by: &self.delegated_by,
                    delegation_id,
                    context_id: None,
                    status: LedgerStatus::InProgress,
                    outcome: None,
                    blocked_reason: None,
                    result_summary: None,
                })
                .await
            {
                Ok(ledger_task_id) => LedgerStart::Linked { ledger_task_id },
                Err(e) => {
                    warn("create", &e);
                    LedgerStart::Unlinked
                }
            }
        }

        /// Settle the row when a background delegation ends.
        pub(crate) async fn finish(&self, delegation_id: &str, result: &BackgroundDelegateResult) {
            let Some(end) = end_for_result(result) else {
                return;
            };
            match self
                .ledger
                .finish_delegation_returning(delegation_id, end)
                .await
            {
                // `None` = nothing to settle (already archived by the agent, or
                // stopped by the operator): nothing happened, so nothing to announce.
                Ok(Some(task)) => spawn_notify(&self.notify, task),
                Ok(None) => {}
                Err(e) => warn("finish", &e),
            }
        }

        /// Record a sync hop that failed, after the fact. Returns the row id.
        pub(crate) async fn record_failed_hop(
            &self,
            delegation_id: &str,
            agent: &str,
            prompt: &str,
            reason: DelegateReason,
            error: &str,
        ) -> Option<String> {
            let outcome = match reason {
                DelegateReason::TimedOut => TaskOutcome::TimedOut,
                DelegateReason::Lost => TaskOutcome::Lost,
                _ => TaskOutcome::Failed,
            };
            let blocked = failure_text(reason, error);
            let title = title_for(agent, prompt);
            match self
                .ledger
                .create_delegated_task(NewDelegatedTask {
                    tenant_id: &self.tenant_id,
                    agent_type: &self.agent_type,
                    session_id: self.session_id.as_deref(),
                    title: &title,
                    delegated_by: &self.delegated_by,
                    delegation_id,
                    context_id: None,
                    status: LedgerStatus::Blocked,
                    outcome: Some(outcome),
                    blocked_reason: Some(&blocked),
                    result_summary: None,
                })
                .await
            {
                Ok(id) => {
                    // A sync hop that failed is now `blocked`: tell the operator, as
                    // an agent's own `task_update_status blocked` would.
                    if let Ok(Some(task)) = self.ledger.get_task(&self.tenant_id, &id).await {
                        spawn_notify(&self.notify, task);
                    }
                    Some(id)
                }
                Err(e) => {
                    warn("record_failed_hop", &e);
                    None
                }
            }
        }

        /// Resolves once the operator has stopped this delegation's row.
        /// Never resolves if the row is gone (finished and archived) — the
        /// delegation's own completion ends the race in that case.
        pub(crate) async fn watch_cancel(self, delegation_id: String) {
            loop {
                tokio::time::sleep(CANCEL_POLL).await;
                match self.ledger.delegation_status(&delegation_id).await {
                    Ok(Some(LedgerStatus::Cancelled)) => return,
                    Ok(Some(_)) => {}
                    Ok(None) => std::future::pending::<()>().await,
                    Err(_) => {}
                }
            }
        }
    }

    /// Settle rows still shown `in_progress` whose delegation the registry says
    /// has ended. Idempotent with the engine's own `finish` (which runs first,
    /// before the registry is updated), so a row is never settled with less
    /// than the engine would have written.
    pub(crate) async fn reconcile(store: &dyn TaskRegistry) {
        reconcile_with(store, crate::cron::tenant_sync::backend()).await;
    }

    /// [`reconcile`] with the notification target passed in, so it can be tested
    /// without touching process environment.
    pub(crate) async fn reconcile_with(store: &dyn TaskRegistry, notify: Option<(String, String)>) {
        let Some(ledger) = current_task_ledger() else {
            return;
        };
        let ids = match ledger.open_delegation_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                warn("open_delegation_ids", &e);
                return;
            }
        };
        for id in ids {
            let Ok(Some(record)) = store.get(&id).await else {
                continue;
            };
            let Some(end) = end_for_registry_status(record.status) else {
                continue;
            };
            match ledger.finish_delegation_returning(&id, end).await {
                Ok(Some(task)) => spawn_notify(&notify, task),
                Ok(None) => {}
                Err(e) => warn("reconcile", &e),
            }
        }
    }

    /// Announce a settled delegated task on avry-backend's activity feed --
    /// fire-and-forget, so the write that settled it never waits on an HTTP call.
    /// Only a move into `blocked` or `done` is announced (`cancelled` is the
    /// operator's or the caller's own action), exactly as for an agent's tool call.
    fn spawn_notify(
        notify: &Option<(String, String)>,
        task: zeroclaw_memory::task_ledger::AgentTask,
    ) {
        let Some((base_url, token)) = notify.clone() else {
            return;
        };
        if !zeroclaw_tools::task_ledger::transition_notifies(task.status) {
            return;
        }
        zeroclaw_spawn::spawn!(async move {
            zeroclaw_tools::task_ledger::notify_task_transition(&base_url, &token, &task).await;
        });
    }

    /// The text a person reads on a failed delegation's row.
    pub(crate) fn failure_text(reason: DelegateReason, error: &str) -> String {
        let line = first_line(error);
        match reason {
            DelegateReason::TimedOut => format!("Delegation timed out: {line}"),
            DelegateReason::Lost => {
                "Delegation lost: the daemon restarted while it was running".into()
            }
            _ => format!("Delegation failed: {line}"),
        }
    }

    pub(crate) fn title_for(agent: &str, prompt: &str) -> String {
        let line = first_line(prompt);
        let short: String = line.chars().take(TITLE_PROMPT_CHARS).collect();
        let ellipsis = if line.chars().count() > TITLE_PROMPT_CHARS {
            "…"
        } else {
            ""
        };
        format!("Delegated to {agent}: {short}{ellipsis}")
    }

    fn first_line(text: &str) -> String {
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        line.chars().take(REASON_CHARS).collect()
    }

    fn adopt_refusal(task_id: &str, result: AdoptResult) -> String {
        match result {
            AdoptResult::Cancelled => format!(
                "ledger_task_id {task_id} was stopped by the operator; do not resume it. \
                 Create a new task if the work is still wanted."
            ),
            AdoptResult::Done => format!(
                "ledger_task_id {task_id} is already done; `done` is terminal. Create a new task."
            ),
            AdoptResult::AlreadyDelegated => format!(
                "ledger_task_id {task_id} already tracks another delegation; create a new task."
            ),
            AdoptResult::NotFound | AdoptResult::Adopted => {
                format!("ledger_task_id {task_id} was not found for this tenant")
            }
        }
    }

    /// Map a stored background result to how its row should end. `None` while
    /// the task is still running.
    pub(crate) fn end_for_result(result: &BackgroundDelegateResult) -> Option<DelegationEnd> {
        let reason = result
            .meta
            .as_ref()
            .and_then(|m| m.reason.as_deref())
            .and_then(DelegateReason::parse);
        let error = result.error.as_deref().unwrap_or("");
        match result.status {
            BackgroundTaskStatus::Running => None,
            BackgroundTaskStatus::Completed => Some(DelegationEnd::Completed {
                summary: result
                    .output
                    .as_deref()
                    .map(|o| envelope::strip_agent_header(o).trim().to_string())
                    .filter(|s| !s.is_empty()),
            }),
            BackgroundTaskStatus::InputRequired => {
                let reason = match result.meta.as_ref() {
                    Some(meta) => match (&meta.approval_tool, &meta.approval_id) {
                        (Some(tool), Some(id)) => {
                            format!("Waiting for approval: {tool} ({id})")
                        }
                        _ => "Waiting for approval".to_string(),
                    },
                    None => "Waiting for approval".to_string(),
                };
                Some(DelegationEnd::InputRequired { reason })
            }
            BackgroundTaskStatus::Failed => {
                let reason = reason.unwrap_or(DelegateReason::ToolError);
                let outcome = match reason {
                    DelegateReason::TimedOut => TaskOutcome::TimedOut,
                    DelegateReason::Lost => TaskOutcome::Lost,
                    _ => TaskOutcome::Failed,
                };
                Some(DelegationEnd::Failed {
                    outcome,
                    reason: failure_text(reason, error),
                })
            }
            BackgroundTaskStatus::Cancelled => Some(DelegationEnd::Cancelled {
                reason: if error.is_empty() {
                    "Cancelled".to_string()
                } else {
                    first_line(error)
                },
            }),
        }
    }

    /// Map the registry's verdict on a delegation to how its row should end.
    /// `None` while it is still running.
    pub(crate) fn end_for_registry_status(status: TaskStatus) -> Option<DelegationEnd> {
        Some(match status {
            TaskStatus::Running => return None,
            TaskStatus::Paused => DelegationEnd::InputRequired {
                reason: "Waiting for approval".into(),
            },
            TaskStatus::Completed => DelegationEnd::Completed { summary: None },
            TaskStatus::Cancelled => DelegationEnd::Cancelled {
                reason: "Cancelled".into(),
            },
            TaskStatus::Failed => DelegationEnd::Failed {
                outcome: TaskOutcome::Failed,
                reason: failure_text(DelegateReason::ToolError, "the delegation failed"),
            },
            TaskStatus::TimedOut => DelegationEnd::Failed {
                outcome: TaskOutcome::TimedOut,
                reason: failure_text(
                    DelegateReason::TimedOut,
                    "the delegation exceeded its runtime",
                ),
            },
            TaskStatus::Lost => DelegationEnd::Failed {
                outcome: TaskOutcome::Lost,
                reason: failure_text(DelegateReason::Lost, ""),
            },
        })
    }

    fn warn(op: &str, error: &anyhow::Error) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({ "op": op, "error": format!("{error:#}") })),
            "delegation ledger link failed (non-fatal; the delegation is unaffected)"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tools::delegate::BackgroundResultMeta;

        fn result(
            status: BackgroundTaskStatus,
            output: Option<&str>,
            error: Option<&str>,
            meta: Option<BackgroundResultMeta>,
        ) -> BackgroundDelegateResult {
            BackgroundDelegateResult {
                task_id: "t".into(),
                agent: "lex".into(),
                status,
                output: output.map(str::to_string),
                error: error.map(str::to_string),
                started_at: "2026-09-19T10:00:00Z".into(),
                finished_at: Some("2026-09-19T10:00:05Z".into()),
                meta,
            }
        }

        fn meta(reason: &str) -> BackgroundResultMeta {
            BackgroundResultMeta {
                reason: Some(reason.into()),
                ..BackgroundResultMeta::default()
            }
        }

        #[test]
        fn a_running_task_has_no_end() {
            assert!(
                end_for_result(&result(BackgroundTaskStatus::Running, None, None, None)).is_none()
            );
            assert!(end_for_registry_status(TaskStatus::Running).is_none());
        }

        #[test]
        fn completed_carries_the_answer_without_the_agent_label() {
            let end = end_for_result(&result(
                BackgroundTaskStatus::Completed,
                Some("[Agent 'lex' (p/m)]\nqualified 3 leads"),
                None,
                None,
            ));
            match end {
                Some(DelegationEnd::Completed { summary }) => {
                    assert_eq!(summary.as_deref(), Some("qualified 3 leads"));
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn failure_outcome_follows_the_recorded_reason() {
            let timed = end_for_result(&result(
                BackgroundTaskStatus::Failed,
                None,
                Some("Agent 'lex' timed out after 300s"),
                Some(meta("timed_out")),
            ));
            match timed {
                Some(DelegationEnd::Failed { outcome, reason }) => {
                    assert_eq!(outcome, TaskOutcome::TimedOut);
                    assert!(reason.starts_with("Delegation timed out: Agent 'lex' timed out"));
                }
                other => panic!("{other:?}"),
            }
            // A file written before A1 has no meta: still a well-formed failure.
            let legacy = end_for_result(&result(
                BackgroundTaskStatus::Failed,
                None,
                Some("boom\nat frame 1"),
                None,
            ));
            match legacy {
                Some(DelegationEnd::Failed { outcome, reason }) => {
                    assert_eq!(outcome, TaskOutcome::Failed);
                    assert_eq!(reason, "Delegation failed: boom");
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn input_required_names_the_tool_and_approval() {
            let end = end_for_result(&result(
                BackgroundTaskStatus::InputRequired,
                Some("parked"),
                Some("Waiting on approval pa_9 for tool send_email"),
                Some(BackgroundResultMeta {
                    reason: Some("approval_pending".into()),
                    approval_id: Some("pa_9".into()),
                    approval_tool: Some("send_email".into()),
                }),
            ));
            match end {
                Some(DelegationEnd::InputRequired { reason }) => {
                    assert_eq!(reason, "Waiting for approval: send_email (pa_9)");
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn cancelled_keeps_the_first_line_of_the_reason() {
            let end = end_for_result(&result(
                BackgroundTaskStatus::Cancelled,
                None,
                Some("Cancelled by operator (stopped from Mission Control)"),
                None,
            ));
            match end {
                Some(DelegationEnd::Cancelled { reason }) => {
                    assert!(reason.starts_with("Cancelled by operator"));
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn registry_verdicts_map_to_the_matching_end() {
            assert!(matches!(
                end_for_registry_status(TaskStatus::Lost),
                Some(DelegationEnd::Failed {
                    outcome: TaskOutcome::Lost,
                    ..
                })
            ));
            assert!(matches!(
                end_for_registry_status(TaskStatus::TimedOut),
                Some(DelegationEnd::Failed {
                    outcome: TaskOutcome::TimedOut,
                    ..
                })
            ));
            assert!(matches!(
                end_for_registry_status(TaskStatus::Failed),
                Some(DelegationEnd::Failed {
                    outcome: TaskOutcome::Failed,
                    ..
                })
            ));
            assert!(matches!(
                end_for_registry_status(TaskStatus::Paused),
                Some(DelegationEnd::InputRequired { .. })
            ));
            assert!(matches!(
                end_for_registry_status(TaskStatus::Cancelled),
                Some(DelegationEnd::Cancelled { .. })
            ));
            assert!(matches!(
                end_for_registry_status(TaskStatus::Completed),
                Some(DelegationEnd::Completed { summary: None })
            ));
        }

        #[test]
        fn lost_text_names_the_restart_not_the_error() {
            assert_eq!(
                failure_text(DelegateReason::Lost, ""),
                "Delegation lost: the daemon restarted while it was running"
            );
        }

        #[test]
        fn title_is_one_bounded_line() {
            assert_eq!(
                title_for("lex", "Qualify the new leads\nsecond line"),
                "Delegated to lex: Qualify the new leads"
            );
            let long = "x".repeat(200);
            let title = title_for("lex", &long);
            assert!(title.ends_with('…'));
            assert!(title.chars().count() < 110);
        }

        #[test]
        fn every_adopt_refusal_says_what_to_do_next() {
            for result in [
                AdoptResult::Cancelled,
                AdoptResult::Done,
                AdoptResult::AlreadyDelegated,
                AdoptResult::NotFound,
            ] {
                let text = adopt_refusal("abc", result);
                assert!(text.contains("abc"), "{text}");
            }
            assert!(adopt_refusal("abc", AdoptResult::Cancelled).contains("do not resume"));
            assert!(adopt_refusal("abc", AdoptResult::Done).contains("Create a new task"));
        }
    }
}

#[cfg(not(feature = "memory-postgres"))]
mod stub {
    use super::LedgerStart;
    use crate::control_plane::TaskRegistry;
    use crate::tools::delegate::BackgroundDelegateResult;
    use crate::tools::delegate_envelope::DelegateReason;

    #[derive(Clone)]
    pub(crate) struct LedgerLink;

    #[allow(dead_code)]
    impl LedgerLink {
        pub(crate) fn capture(_target: &str) -> Option<Self> {
            None
        }
        pub(crate) async fn start_background(
            &self,
            _delegation_id: &str,
            _agent: &str,
            _prompt: &str,
            _adopt: Option<&str>,
        ) -> LedgerStart {
            LedgerStart::Unlinked
        }
        pub(crate) async fn finish(&self, _id: &str, _result: &BackgroundDelegateResult) {}
        pub(crate) async fn record_failed_hop(
            &self,
            _delegation_id: &str,
            _agent: &str,
            _prompt: &str,
            _reason: DelegateReason,
            _error: &str,
        ) -> Option<String> {
            None
        }
        pub(crate) async fn watch_cancel(self, _delegation_id: String) {
            std::future::pending::<()>().await
        }
    }

    pub(crate) async fn reconcile(_store: &dyn TaskRegistry) {}
}
