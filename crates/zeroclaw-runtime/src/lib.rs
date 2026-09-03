#![allow(
    clippy::to_string_in_format_args,
    clippy::useless_format,
    clippy::manual_inspect
)]
// ADR-009 Phase 1: `cron::scheduler::run_agent_job` now wraps its
// `agent::run` call in an extra `TENANT_CONTEXT.scope(...)` (the same
// task-local nesting shape `zeroclaw-gateway`'s webhook/approval-resume
// paths already use around `process_message`), which pushes the
// Send-auto-trait recursion needed to type-check the resulting deeply
// nested `TaskLocalFuture<..., Instrumented<...>>` chain past the default
// limit (`overflow evaluating the requirement ...: Send`, exactly the
// error rustc suggests this fix for) — not a real cycle, just more nested
// generic layers than the default budget expects.
#![recursion_limit = "256"]
//! Agent runtime — orchestration, security, observability, cron, SOP, skills, hardware, and more.

pub mod cli_input;
pub mod identity;
pub mod migration;
pub mod util;

pub mod agent;
pub mod approval;
pub mod browse;
pub mod calendar;
pub mod control_plane;
pub mod cost;
pub mod cron;
pub mod daemon;
pub mod doctor;
pub mod health;
pub mod heartbeat;
pub mod hooks;
pub mod i18n;
pub mod integrations;
pub mod nodes;
pub mod observability;
pub mod peers;
pub mod platform;
pub mod process_stats;
pub mod quickstart;
pub mod rag;
pub mod restart;
pub mod routines;
pub mod rpc;
pub mod security;
pub mod service;
pub mod skillforge;
pub mod skills;
pub mod sop;
pub mod subagent;
pub mod tools;
pub mod trust;
pub mod tunnel;
pub mod verifiable_intent;
