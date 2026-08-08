//! F-1: boot-time auto-resume for goal tasks that survived a daemon crash with
//! a persisted [`TaskContinuationContext`] (ADR-003 — "F-1 — Auto-resume of
//! crashed in-flight tasks").
//!
//! `reaper::recovery_pass` already does the safe, unconditional half: it
//! durably parks every such candidate at `TaskStatus::Paused` /
//! [`GoalPauseReason::DaemonRestart`] instead of reconciling it to `Lost` —
//! that alone is a strict improvement (a real, inspectable, resumable state)
//! independent of anything below. This module is the best-effort AUTOMATIC
//! half: for each candidate, replay exactly one trusted continuation turn
//! through the same primitives `cron::scheduler` already proves work for a
//! runtime-initiated (non-interactive) turn — `agent::run` plus the
//! `register_delivery_fn` channel-delivery hook — so no new crate-graph
//! inversion is needed here (`zeroclaw-channels` depends on `zeroclaw-runtime`,
//! not the reverse; `zeroclaw-runtime` already runs full turns and delivers
//! their output without ever importing `zeroclaw-channels`, exactly as cron
//! jobs do today).
//!
//! Gated by the F-2 idempotency ledger ([`tool_idem`]) so a second crash mid-
//! drive can never fire the same continuation twice — the ledger key is bound
//! to `(task_id, crashed_boot_id)`, i.e. to the SPECIFIC orphaned run being
//! recovered, not to this boot, so recovering the same orphan again on a
//! third boot (because the second boot itself died mid-drive) still resolves
//! to the same claim instead of minting a fresh one.

use std::path::PathBuf;
use std::sync::Arc;

use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_config::schema::Config;

use super::boot::ControlPlaneHandle;
use super::goal_task::{
    GoalBlocker, GoalBlockerKind, GoalPauseReason, GoalPauseState, TaskContinuationContext,
    TaskContinuationConversationScope,
};
use super::reaper::ResumableGoal;
use super::task_registry::TaskStatus;
use super::tool_idem::{Claim, ToolIdemLedger};

/// Outcome of one boot's [`drive_resumable_goals`] pass — surfaced for logging
/// and tests; never propagated as an error, matching the reaper's own
/// "log, never fail the daemon" discipline for background supervision work.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DriveOutcome {
    /// Continuation turn ran and completed; task marked `Completed`.
    pub resumed: usize,
    /// Turn errored (or `resume_goal_task` itself failed); task re-parked
    /// `Paused`/`DaemonRestart`, retryable on a later boot.
    pub re_paused: usize,
    /// F-2 claim was `InFlight` — a previous boot started but never finished
    /// driving this exact orphan. Not retried automatically; re-parked for
    /// operator visibility instead.
    pub interrupted: usize,
    /// F-2 claim was `AlreadyDone` — defensive backstop, should not normally
    /// occur given ownership-claim ordering. Left untouched, just logged.
    pub already_done: usize,
}

/// Mirrors `zeroclaw_channels::orchestrator::conversation_history_key`'s
/// general-case formula. Kept in sync BY HAND, not by calling it — same
/// crate-graph-inversion reason as `install_mcp_sandbox_hook` /
/// `install_tenant_id_provider` (`zeroclaw-channels` depends on
/// `zeroclaw-runtime`, so the reverse call is impossible). If that canonical
/// implementation's formula ever changes, this must be updated to match or a
/// resumed goal will hydrate the wrong (or no) prior history.
///
/// KNOWN, DELIBERATE GAP: the canonical implementation additionally special-
/// cases Matrix (`thread_ts` is a delivery anchor there, not a topic
/// boundary — root and follow-ups share one session despite having distinct
/// `thread_ts` values). That per-channel override is not reproduced here —
/// this crate has no reason to depend on Matrix-specific channel internals
/// for a feature no producer exercises yet. A Matrix goal continuation would
/// therefore key its session slightly differently than a live Matrix turn
/// would. Flagged rather than silently risked; revisit if/when a real goal
/// producer targets Matrix.
fn continuation_session_key(ctx: &TaskContinuationContext) -> String {
    let channel_scope = match ctx.channel_alias.as_deref() {
        Some(alias) if !alias.is_empty() => format!("{}.{}", ctx.channel, alias),
        _ => ctx.channel.clone(),
    };
    let raw = match (ctx.conversation_scope, ctx.thread_ts.as_deref()) {
        (TaskContinuationConversationScope::ReplyTarget, _) => {
            format!("{channel_scope}_{}", ctx.reply_target)
        }
        (TaskContinuationConversationScope::Sender, Some(tid)) => {
            format!("{channel_scope}_{}_{tid}_{}", ctx.reply_target, ctx.sender)
        }
        (TaskContinuationConversationScope::Sender, None) => {
            format!("{channel_scope}_{}_{}", ctx.reply_target, ctx.sender)
        }
    };
    zeroclaw_api::session_keys::sanitize_session_key(&raw)
}

/// `deliver_announcement`'s channel argument must be a dotted `<type>.<alias>`
/// ref. `TaskContinuationContext.channel_alias` is optional (per-channel:
/// "when multiple bots share the same channel family"); default to
/// `"default"` when absent rather than failing delivery outright.
fn channel_ref(ctx: &TaskContinuationContext) -> String {
    match ctx.channel_alias.as_deref() {
        Some(alias) if !alias.is_empty() => format!("{}.{}", ctx.channel, alias),
        _ => format!("{}.default", ctx.channel),
    }
}

fn resume_idem_key(task_id: &str, crashed_boot_id: &str) -> String {
    super::tool_idem::derive_key("", task_id, crashed_boot_id, "f1_goal_resume", "")
}

async fn re_park(
    handle: &ControlPlaneHandle,
    task_id: &str,
    reason_detail: String,
) -> anyhow::Result<()> {
    handle
        .goal_store
        .pause_goal_task(
            task_id,
            GoalPauseState {
                reason: GoalPauseReason::DaemonRestart,
                description: Some(reason_detail),
                blockers: vec![GoalBlocker {
                    kind: GoalBlockerKind::RestartRecovery,
                    message: "daemon restarted mid-task".into(),
                    payload: None,
                }],
            },
        )
        .await
}

/// Drive every candidate `recovery_pass` collected this boot. Best-effort:
/// one candidate's failure never stops the rest, and nothing here is
/// propagated as a fatal daemon-startup error — mirrors `reaper_loop`'s own
/// "log, never take down the daemon" posture for background supervision.
pub async fn drive_resumable_goals(handle: &ControlPlaneHandle, config: &Config) -> DriveOutcome {
    let mut outcome = DriveOutcome::default();
    let ledger = match ToolIdemLedger::shared(&config.data_dir) {
        Ok(l) => l,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({ "error": format!("{e:#}") })),
                "F-1: could not open the F-2 idempotency ledger; skipping auto-resume this boot \
                 (candidates stay durably Paused/DaemonRestart, unaffected)"
            );
            return outcome;
        }
    };
    for goal in &handle.resumable_goals {
        drive_one(handle, config, &ledger, goal, &mut outcome).await;
    }
    outcome
}

async fn drive_one(
    handle: &ControlPlaneHandle,
    config: &Config,
    ledger: &Arc<ToolIdemLedger>,
    goal: &ResumableGoal,
    outcome: &mut DriveOutcome,
) {
    let task_id = goal.task.id.as_str();
    let key = resume_idem_key(task_id, &goal.crashed_boot_id);

    let claim = match ledger.claim(&key) {
        Ok(c) => c,
        Err(e) => {
            log_drive_note(task_id, "f2_claim_failed", &format!("{e:#}"));
            return;
        }
    };
    match claim {
        Claim::AlreadyDone(_) => {
            outcome.already_done += 1;
            log_drive_note(
                task_id,
                "already_done",
                "F-2 ledger already has a completed resume for this orphan; leaving state as-is",
            );
        }
        Claim::InFlight => {
            outcome.interrupted += 1;
            if let Err(e) = re_park(
                handle,
                task_id,
                "a previous automatic resume attempt was interrupted before completing; \
                 needs operator review before retrying"
                    .into(),
            )
            .await
            {
                log_drive_note(task_id, "re_park_failed_after_in_flight", &format!("{e:#}"));
            }
        }
        Claim::Claimed => drive_claimed(handle, config, ledger, goal, &key, outcome).await,
    }
}

async fn drive_claimed(
    handle: &ControlPlaneHandle,
    config: &Config,
    ledger: &Arc<ToolIdemLedger>,
    goal: &ResumableGoal,
    key: &str,
    outcome: &mut DriveOutcome,
) {
    let task_id = goal.task.id.as_str();

    let objective = match handle.goal_store.get_goal_task(task_id).await {
        Ok(Some(g)) => g.objective,
        Ok(None) => {
            let _ = ledger.release(key);
            log_drive_note(task_id, "missing_goal_extension", "no goal extension row");
            return;
        }
        Err(e) => {
            let _ = ledger.release(key);
            log_drive_note(task_id, "load_goal_failed", &format!("{e:#}"));
            return;
        }
    };

    if let Err(e) = handle
        .goal_store
        .resume_goal_task(
            task_id,
            std::process::id(),
            &handle.boot_id,
            Some(goal.context.clone()),
        )
        .await
    {
        let _ = ledger.release(key);
        log_drive_note(task_id, "claim_ownership_failed", &format!("{e:#}"));
        return;
    }

    let prompt = format!(
        "[goal-resume:{task_id}] The daemon restarted while this goal was in progress. \
         Continue the following objective from where it left off: {objective}"
    );
    let session_key = continuation_session_key(&goal.context);

    let run_result = crate::agent::run(
        config.clone(),
        goal.task.agent.as_str(),
        Some(prompt),
        None,
        None,
        None,
        vec![],
        false,
        Some(PathBuf::from(session_key)),
        None,
        TurnOrigin::Daemon,
        crate::agent::loop_::AgentRunOverrides::default(),
    )
    .await;

    match run_result {
        Ok(response) => {
            if let Err(e) = handle
                .store
                .update_status(task_id, TaskStatus::Completed, Some(response.clone()), None)
                .await
            {
                log_drive_note(task_id, "mark_completed_failed", &format!("{e:#}"));
            }
            if let Err(e) = ledger.complete(key, &response) {
                log_drive_note(task_id, "f2_complete_failed", &format!("{e:#}"));
            }
            outcome.resumed += 1;

            if let Err(e) = crate::cron::scheduler::deliver_announcement(
                config,
                &channel_ref(&goal.context),
                &goal.context.reply_target,
                goal.context.thread_ts.as_deref(),
                &response,
            )
            .await
            {
                // Delivery failing does not undo the completed, ledgered turn —
                // logged only, matching `deliver_if_configured`'s own posture
                // for a cron job whose delivery step fails after a real run.
                log_drive_note(task_id, "delivery_failed", &format!("{e:#}"));
            }
        }
        Err(e) => {
            let _ = ledger.release(key);
            outcome.re_paused += 1;
            if let Err(pe) = re_park(
                handle,
                task_id,
                format!("automatic resume attempt failed: {e:#}"),
            )
            .await
            {
                log_drive_note(task_id, "re_park_failed_after_turn_error", &format!("{pe:#}"));
            }
        }
    }
}

fn log_drive_note(task_id: &str, kind: &str, detail: &str) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "task_id": task_id, "kind": kind, "detail": detail })),
        "F-1 goal auto-resume: notable event during drive"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::goal_task::GoalTaskRecord;
    use crate::control_plane::task_registry::{TaskKind, TaskRecord, TaskRegistry};
    use crate::control_plane::task_store_sqlite::SqliteTaskStore;

    fn ctx() -> TaskContinuationContext {
        TaskContinuationContext {
            channel: "telegram".into(),
            channel_alias: Some("main".into()),
            reply_target: "chat-1".into(),
            sender: "alice".into(),
            thread_ts: None,
            interruption_scope_id: None,
            conversation_scope: TaskContinuationConversationScope::Sender,
        }
    }

    #[test]
    fn session_key_is_stable_and_scope_sensitive() {
        let a = continuation_session_key(&ctx());
        let a2 = continuation_session_key(&ctx());
        assert_eq!(a, a2);

        let mut reply_scoped = ctx();
        reply_scoped.conversation_scope = TaskContinuationConversationScope::ReplyTarget;
        assert_ne!(a, continuation_session_key(&reply_scoped));

        let mut other_sender = ctx();
        other_sender.sender = "bob".into();
        assert_ne!(a, continuation_session_key(&other_sender));
    }

    #[test]
    fn channel_ref_defaults_alias_when_absent() {
        let mut c = ctx();
        c.channel_alias = None;
        assert_eq!(channel_ref(&c), "telegram.default");
        assert_eq!(channel_ref(&ctx()), "telegram.main");
    }

    #[test]
    fn resume_idem_key_binds_to_the_crashed_boot_not_the_recovering_one() {
        let k1 = resume_idem_key("task-1", "boot-OLD-1");
        let k2 = resume_idem_key("task-1", "boot-OLD-2");
        assert_ne!(
            k1, k2,
            "two different crashed boots for the same task must not collide"
        );
        assert_eq!(k1, resume_idem_key("task-1", "boot-OLD-1"));
    }

    /// F-2 gating alone, independent of any real turn: claiming twice for the
    /// same (task_id, crashed_boot_id) must behave exactly like F-2's own
    /// documented contract (`InFlight` until completed, then `AlreadyDone`).
    #[tokio::test]
    async fn ledger_claim_lifecycle_matches_f2_contract_for_a_resume_key() {
        let ledger = ToolIdemLedger::new_in_memory().unwrap();
        let key = resume_idem_key("task-1", "boot-OLD");
        assert_eq!(ledger.claim(&key).unwrap(), Claim::Claimed);
        assert_eq!(
            ledger.claim(&key).unwrap(),
            Claim::InFlight,
            "a second recovery of the same orphan before completion must not double-fire"
        );
        ledger.complete(&key, "resumed ok").unwrap();
        assert_eq!(
            ledger.claim(&key).unwrap(),
            Claim::AlreadyDone("resumed ok".into())
        );
    }

    #[tokio::test]
    async fn drive_resumable_goals_is_a_noop_when_nothing_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite = Arc::new(SqliteTaskStore::new(dir.path()).unwrap());
        let store: Arc<dyn TaskRegistry> = sqlite.clone();
        let goal_store: Arc<dyn super::super::goal_task::GoalTaskRegistry> = sqlite;
        let handle = ControlPlaneHandle {
            store,
            goal_store,
            boot_id: "boot-NEW".into(),
            resumable_goals: Vec::new(),
        };
        let config = Config::default();
        let outcome = drive_resumable_goals(&handle, &config).await;
        assert_eq!(outcome, DriveOutcome::default());
    }

    /// `agent::run` needs a fully-resolved agent alias/provider to actually
    /// execute a turn, which this unit test deliberately does not set up
    /// (per this session's "don't spend real LLM cost / real turns in tests"
    /// instruction — see the integration-level non-LLM harness in
    /// `docs/CERVEAU-STATUS.md` for how F-1 gets verified against a real
    /// config without ever calling an LLM). This test instead proves the
    /// SAFE-DEGRADATION contract that matters most for a boot-time path: a
    /// candidate whose turn cannot even start must land back at a durable,
    /// resumable `Paused`/`DaemonRestart` state — never stuck at bare
    /// `Running` with nothing watching it — and the F-2 claim must be
    /// released (not completed) so a later boot can retry.
    #[tokio::test]
    async fn a_candidate_that_cannot_run_is_re_parked_not_left_running() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite = Arc::new(SqliteTaskStore::new(dir.path()).unwrap());
        let store: Arc<dyn TaskRegistry> = sqlite.clone();
        let goal_store: Arc<dyn super::super::goal_task::GoalTaskRegistry> = sqlite;

        let mut task = TaskRecord {
            id: "goal-cannot-run".into(),
            kind: TaskKind::Goal,
            agent: "agent-that-does-not-exist-in-config".into(),
            status: TaskStatus::Running,
            owner_pid: 999_999,
            owner_boot_id: "boot-OLD".into(),
            heartbeat_at: None,
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            started_at: "2026-06-18T00:00:00Z".into(),
            finished_at: None,
        };
        task.status = TaskStatus::Paused; // recovery_pass already parked it
        goal_store
            .create_goal(
                task.clone(),
                GoalTaskRecord {
                    task_id: "goal-cannot-run".into(),
                    objective: "an objective".into(),
                    effective_token_limit: None,
                    effective_cost_limit_usd: None,
                    pause_reason: Some(GoalPauseReason::DaemonRestart),
                    pause_description: None,
                    blockers: Vec::new(),
                },
                Some(ctx()),
            )
            .await
            .unwrap();

        let candidate = ResumableGoal {
            task,
            context: ctx(),
            crashed_boot_id: "boot-OLD".into(),
        };
        let handle = ControlPlaneHandle {
            store,
            goal_store,
            boot_id: "boot-NEW".into(),
            resumable_goals: vec![candidate.clone()],
        };
        // `ToolIdemLedger::shared` is a process-wide singleton keyed by
        // whichever `data_dir` wins the race across this whole test binary
        // (other suites, e.g. `agent::loop_`'s F-2 wiring tests, also call
        // it) — so this test cannot assume `config.data_dir` is the instance
        // it actually lands on. It proves "released, not completed" without
        // touching the ledger directly: driving the SAME candidate twice must
        // attempt (and re-park) BOTH times, never skip the second attempt as
        // `AlreadyDone` — that would only happen if the first attempt's claim
        // had been released rather than completed.
        let config = {
            let mut c = Config::default();
            c.data_dir = dir.path().to_path_buf();
            c
        };

        let outcome = drive_resumable_goals(&handle, &config).await;
        assert_eq!(outcome.re_paused, 1);
        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.already_done, 0);

        let final_task = handle.store.get("goal-cannot-run").await.unwrap().unwrap();
        assert_eq!(
            final_task.status,
            TaskStatus::Paused,
            "a failed resume attempt must never leave the task at bare Running"
        );
        let final_goal = handle
            .goal_store
            .get_goal_task("goal-cannot-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_goal.pause_reason, Some(GoalPauseReason::DaemonRestart));

        // Second drive of the identical (task_id, crashed_boot_id) candidate,
        // simulating this same orphan surfacing again on a later boot's
        // recovery pass. If the first failure had wrongly `complete()`d the
        // F-2 claim instead of `release()`ing it, this would report
        // `already_done: 1` and `re_paused: 0` instead.
        let handle2 = ControlPlaneHandle {
            store: handle.store.clone(),
            goal_store: handle.goal_store.clone(),
            boot_id: "boot-NEWER".into(),
            resumable_goals: vec![candidate],
        };
        let outcome2 = drive_resumable_goals(&handle2, &config).await;
        assert_eq!(
            outcome2.re_paused, 1,
            "a released (not completed) claim must be retryable on a later boot"
        );
        assert_eq!(outcome2.already_done, 0);
    }
}
