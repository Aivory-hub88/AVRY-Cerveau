//! The supervision reaper — moves abandoned `Running` tasks to a terminal state
//! from OUTSIDE the task body, which the flat-file design could never do.
//!
//! Two entry points, both modelled on the ACP idle-reaper
//! (`zeroclaw_channels::orchestrator::acp_server` — `interval(60s)` + lock-aware
//! skip):
//!   * [`recovery_pass`] — a one-shot sweep at boot that reclaims prior-boot orphans.
//!   * [`reaper_loop`] — the periodic sweep that also times out the daemon's own
//!     hung tasks.
//!
//! Safety: reclamation goes through [`TaskRegistry::reconcile_lost`], which itself
//! enforces [`super::authority::is_authoritative`] — a live same-boot owner's
//! heart-beating task is never reclaimed (the split-brain guard).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::authority::is_authoritative;
use super::goal_task::{
    GoalBlocker, GoalBlockerKind, GoalPauseReason, GoalPauseState, GoalTaskRegistry,
    TaskContinuationContext,
};
use super::task_registry::{TaskKind, TaskRecord, TaskRegistry, TaskStatus};

/// How often the periodic sweep runs.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);
/// Default grace before a same-boot task with a stale/absent heartbeat is timed out.
pub const DEFAULT_MAX_RUNTIME_SECS: i64 = 3600;

/// A prior-boot `Goal` task that [`recovery_pass`] durably parked at
/// `TaskStatus::Paused`/[`GoalPauseReason::DaemonRestart`] (safe, never
/// stranded — see the module doc) rather than reconciling to `Lost`, because
/// it carried a persisted [`TaskContinuationContext`]. `crashed_boot_id` is
/// the ORPHANED boot this candidate belonged to, not the boot doing recovery
/// — `continuation_drive` keys its F-2 claim on this so a repeat recovery of
/// the same orphan (e.g. this boot dies again mid-drive) can't double-fire.
#[derive(Debug, Clone)]
pub struct ResumableGoal {
    pub task: TaskRecord,
    pub context: TaskContinuationContext,
    pub crashed_boot_id: String,
}

/// Outcome of a one-shot [`recovery_pass`].
#[derive(Debug, Default)]
pub struct RecoveryOutcome {
    /// Non-goal (or goal-without-continuation-context) orphans reconciled to `Lost`.
    pub reclaimed: usize,
    /// Goal orphans parked `Paused`/`DaemonRestart`, candidates for F-1 auto-resume.
    pub resumable_goals: Vec<ResumableGoal>,
}

/// Age in seconds of an RFC3339 instant, or `None` if it cannot be parsed. We NEVER
/// reap on a timestamp we could not read — a corrupt `heartbeat_at` must not kill a
/// task (review finding #9).
fn age_secs(ts: &str, now: DateTime<Utc>) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
}

/// One-shot crash-recovery sweep: reclaim every `Running` record left behind by a
/// PRIOR boot. Safe to run at every startup — same-boot records are not yet present
/// (this runs before the reaper spawns) and the authority guard protects any that
/// are.
///
/// F-1 (ADR-003): a `Goal`-kind orphan that carries a persisted
/// [`TaskContinuationContext`] is NOT reconciled to `Lost` like every other
/// orphan — that would discard real resumable state for no reason. Instead it
/// is durably parked `Paused`/[`GoalPauseReason::DaemonRestart`] (a real,
/// inspectable, resumable state — see [`ResumableGoal`]'s doc for why this is
/// safe even if nothing ever drives it further) and returned so a caller with
/// a live `Config` (this module has none) can attempt one automatic
/// continuation turn via `continuation_drive::drive_resumable_goals`. A goal
/// task with no continuation context is unresumable in any useful sense and
/// is reconciled to `Lost` exactly as before.
pub async fn recovery_pass(
    store: &dyn TaskRegistry,
    goal_store: &dyn GoalTaskRegistry,
    boot_id: &str,
) -> anyhow::Result<RecoveryOutcome> {
    let mut outcome = RecoveryOutcome::default();
    for rec in store.list_running().await? {
        if rec.owner_boot_id == boot_id {
            continue;
        }
        if rec.kind == TaskKind::Goal
            && is_authoritative(&rec, boot_id)
            && let Some(context) = goal_store.get_continuation_context(&rec.id).await?
        {
            let crashed_boot_id = rec.owner_boot_id.clone();
            goal_store
                .pause_goal_task(
                    &rec.id,
                    GoalPauseState {
                        reason: GoalPauseReason::DaemonRestart,
                        description: Some(
                            "daemon restarted while this goal was running; queued for \
                             automatic continuation"
                                .into(),
                        ),
                        blockers: vec![GoalBlocker {
                            kind: GoalBlockerKind::RestartRecovery,
                            message: "daemon restarted mid-task".into(),
                            payload: None,
                        }],
                    },
                )
                .await?;
            outcome.resumable_goals.push(ResumableGoal {
                task: rec,
                context,
                crashed_boot_id,
            });
            continue;
        }
        if store.reconcile_lost(&rec.id, boot_id).await? {
            outcome.reclaimed += 1;
        }
    }
    Ok(outcome)
}

pub async fn reaper_loop(
    store: Arc<dyn TaskRegistry>,
    boot_id: String,
    max_runtime_secs: i64,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut tick = tokio::time::interval(REAP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                if let Err(e) = sweep(store.as_ref(), &boot_id, max_runtime_secs).await {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({ "error": format!("{e}") })),
                        "control-plane reaper sweep failed"
                    );
                }
            }
        }
    }
}

/// A single sweep — separated for direct unit testing.
///
/// Deliberately NOT goal-aware (unlike [`recovery_pass`]): F-1 (ADR-003) scopes
/// auto-resume to boot-time recovery, where every prior-boot orphan is known to
/// exist before this daemon's own tasks do. A genuinely different-boot `Goal`
/// orphan surfacing here mid-run would mean a second daemon shares this
/// `data_dir` — already an unsupported configuration per [`super::boot`]'s
/// single-writer invariant — so this path still reconciles such a record to
/// `Lost` rather than growing a second, harder-to-reason-about resume trigger.
pub async fn sweep(
    store: &dyn TaskRegistry,
    boot_id: &str,
    max_runtime_secs: i64,
) -> anyhow::Result<()> {
    let now = Utc::now();
    for rec in store.list_running().await? {
        if rec.owner_boot_id != boot_id {
            // Prior-boot orphan — reclaim (authority-guarded inside reconcile_lost).
            let _ = store.reconcile_lost(&rec.id, boot_id).await?;
        } else {
            if let Some(beat) = rec.heartbeat_at.as_deref()
                && age_secs(beat, now).is_some_and(|age| age > max_runtime_secs)
            {
                store
                    .update_status(
                        &rec.id,
                        TaskStatus::TimedOut,
                        None,
                        Some("heartbeat timeout".into()),
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::task_registry::{TaskKind, TaskRecord};
    use crate::control_plane::task_store_sqlite::SqliteTaskStore;

    fn rec(id: &str, boot: &str, pid: u32, beat_secs_ago: Option<i64>) -> TaskRecord {
        let beat = beat_secs_ago.map(|s| (Utc::now() - chrono::Duration::seconds(s)).to_rfc3339());
        TaskRecord {
            id: id.into(),
            kind: TaskKind::Delegate,
            agent: "main".into(),
            status: TaskStatus::Running,
            owner_pid: pid,
            owner_boot_id: boot.into(),
            heartbeat_at: beat,
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn recovery_reclaims_prior_boot_orphans() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("orphan", "boot-OLD", 999_999, None))
            .await
            .unwrap();
        s.create(rec("mine", "boot-NEW", std::process::id(), Some(0)))
            .await
            .unwrap();
        let outcome = recovery_pass(&s, &s, "boot-NEW").await.unwrap();
        assert_eq!(outcome.reclaimed, 1);
        assert!(outcome.resumable_goals.is_empty());
        assert_eq!(
            s.get("orphan").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );
        assert_eq!(
            s.get("mine").await.unwrap().unwrap().status,
            TaskStatus::Running
        );
    }

    #[tokio::test]
    async fn recovery_parks_goal_orphan_with_continuation_context_instead_of_losing_it() {
        use crate::control_plane::goal_task::{
            GoalPauseReason, GoalTaskRecord, GoalTaskRegistry, TaskContinuationContext,
            TaskContinuationConversationScope,
        };

        let s = SqliteTaskStore::new_in_memory().unwrap();
        let mut goal = rec("goal-orphan", "boot-OLD", 999_999, None);
        goal.kind = TaskKind::Goal;
        let ctx = TaskContinuationContext {
            channel: "telegram".into(),
            channel_alias: Some("main".into()),
            reply_target: "chat-1".into(),
            sender: "alice".into(),
            thread_ts: None,
            interruption_scope_id: None,
            conversation_scope: TaskContinuationConversationScope::Sender,
        };
        s.create_goal(
            goal,
            GoalTaskRecord {
                task_id: "goal-orphan".into(),
                objective: "keep working".into(),
                effective_token_limit: None,
                effective_cost_limit_usd: None,
                pause_reason: None,
                pause_description: None,
                blockers: Vec::new(),
            },
            Some(ctx.clone()),
        )
        .await
        .unwrap();
        // A plain (non-goal) orphan must still be reclaimed exactly as before.
        s.create(rec("plain-orphan", "boot-OLD", 999_998, None))
            .await
            .unwrap();

        let outcome = recovery_pass(&s, &s, "boot-NEW").await.unwrap();

        assert_eq!(outcome.reclaimed, 1, "only the non-goal orphan is Lost");
        assert_eq!(
            s.get("plain-orphan").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );

        assert_eq!(outcome.resumable_goals.len(), 1);
        let resumed = &outcome.resumable_goals[0];
        assert_eq!(resumed.task.id, "goal-orphan");
        assert_eq!(resumed.crashed_boot_id, "boot-OLD");
        assert_eq!(resumed.context, ctx);

        // Durably parked Paused/DaemonRestart, never Lost — real, resumable state.
        let task = s.get("goal-orphan").await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Paused);
        let goal_ext = s.get_goal_task("goal-orphan").await.unwrap().unwrap();
        assert_eq!(goal_ext.pause_reason, Some(GoalPauseReason::DaemonRestart));
    }

    #[tokio::test]
    async fn recovery_still_loses_goal_orphan_without_continuation_context() {
        use crate::control_plane::goal_task::{GoalTaskRecord, GoalTaskRegistry};

        let s = SqliteTaskStore::new_in_memory().unwrap();
        let mut goal = rec("goal-no-ctx", "boot-OLD", 999_999, None);
        goal.kind = TaskKind::Goal;
        s.create_goal(
            goal,
            GoalTaskRecord {
                task_id: "goal-no-ctx".into(),
                objective: "no continuation saved".into(),
                effective_token_limit: None,
                effective_cost_limit_usd: None,
                pause_reason: None,
                pause_description: None,
                blockers: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();

        let outcome = recovery_pass(&s, &s, "boot-NEW").await.unwrap();
        assert_eq!(outcome.reclaimed, 1);
        assert!(outcome.resumable_goals.is_empty());
        assert_eq!(
            s.get("goal-no-ctx").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );
    }

    #[tokio::test]
    async fn recovery_never_touches_live_same_boot_goal_even_with_context() {
        use crate::control_plane::goal_task::{
            GoalTaskRecord, GoalTaskRegistry, TaskContinuationContext,
            TaskContinuationConversationScope,
        };

        let s = SqliteTaskStore::new_in_memory().unwrap();
        let me = std::process::id();
        let mut goal = rec("goal-live", "boot-NEW", me, Some(0));
        goal.kind = TaskKind::Goal;
        s.create_goal(
            goal,
            GoalTaskRecord {
                task_id: "goal-live".into(),
                objective: "still running".into(),
                effective_token_limit: None,
                effective_cost_limit_usd: None,
                pause_reason: None,
                pause_description: None,
                blockers: Vec::new(),
            },
            Some(TaskContinuationContext {
                channel: "telegram".into(),
                channel_alias: None,
                reply_target: "chat-1".into(),
                sender: "alice".into(),
                thread_ts: None,
                interruption_scope_id: None,
                conversation_scope: TaskContinuationConversationScope::Sender,
            }),
        )
        .await
        .unwrap();

        let outcome = recovery_pass(&s, &s, "boot-NEW").await.unwrap();
        assert_eq!(outcome.reclaimed, 0);
        assert!(outcome.resumable_goals.is_empty());
        assert_eq!(
            s.get("goal-live").await.unwrap().unwrap().status,
            TaskStatus::Running,
            "same-boot record must never be touched by its own boot's recovery pass"
        );
    }

    #[tokio::test]
    async fn sweep_times_out_own_stale_task_but_not_fresh() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        let me = std::process::id();
        s.create(rec("stale", "boot-NEW", me, Some(99_999)))
            .await
            .unwrap(); // very old beat
        s.create(rec("fresh", "boot-NEW", me, Some(1)))
            .await
            .unwrap(); // just beat
        sweep(&s, "boot-NEW", 600).await.unwrap();
        assert_eq!(
            s.get("stale").await.unwrap().unwrap().status,
            TaskStatus::TimedOut
        );
        assert_eq!(
            s.get("fresh").await.unwrap().unwrap().status,
            TaskStatus::Running
        );
    }
}
