//! The durable run/task control-plane — supervised lifecycle for delegated and
//! subagent runs.

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

pub use authority::is_authoritative;
pub use boot::ControlPlaneHandle;
pub use continuation_drive::{drive_resumable_goals, DriveOutcome};
pub use global::{control_plane, init_control_plane};
pub use goal_task::{
    GoalBlocker, GoalBlockerKind, GoalPauseReason, GoalPauseState, GoalTaskRecord,
    GoalTaskRegistry, TaskContinuationContext, TaskContinuationConversationScope, TaskGoal,
};
pub use reaper::{RecoveryOutcome, ResumableGoal};
pub use task_registry::{TaskKind, TaskRecord, TaskRegistry, TaskStatus};
pub use task_store_sqlite::SqliteTaskStore;
