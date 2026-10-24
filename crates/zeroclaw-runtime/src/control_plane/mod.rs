//! The durable run/task control-plane — supervised lifecycle for delegated and
//! subagent runs.

pub mod approval_expiry;
pub mod authority;
pub mod boot;
pub mod continuation_drive;
pub mod global;
pub mod goal_task;
pub mod pending_approvals;
pub mod reaper;
pub mod task_registry;
pub mod task_store_sqlite;
pub mod tool_idem;
pub mod verifier_sweep;

pub use authority::is_authoritative;
pub use boot::ControlPlaneHandle;
pub(crate) use boot::ControlPlaneRecoveryOwner;
pub use continuation_drive::{DriveOutcome, drive_resumable_goals};
pub use global::control_plane;
pub(crate) use global::{init_control_plane, non_daemon_control_plane, spawn_control_plane_reaper};
pub use goal_task::{
    GoalBlocker, GoalBlockerKind, GoalPauseReason, GoalPauseState, GoalTaskRecord,
    GoalTaskRegistry, TaskContinuationContext, TaskContinuationConversationScope, TaskGoal,
};
pub use reaper::{RecoveryOutcome, ResumableGoal};
pub use task_registry::{
    TaskKind, TaskProgress, TaskProgressTool, TaskRecord, TaskRegistry, TaskSnapshot, TaskStatus,
};
pub use task_store_sqlite::SqliteTaskStore;
