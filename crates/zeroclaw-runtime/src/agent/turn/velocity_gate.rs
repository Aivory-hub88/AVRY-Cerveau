//! Cross-turn write-velocity gate.
//!
//! The per-turn [`LoopDetector`](super::super::loop_detector) resets on every
//! new turn, so a loop that re-fires once per room message (each turn doing
//! one "successful" write) never trips any in-turn pattern — the failure
//! mode behind the 2026-09-18 Lex incident, where `create_lead`/`update_deal`
//! executed once per round, forever, with zero detector events.
//!
//! This gate closes that hole one layer up: it counts *would-execute*
//! mutating calls per `(tenant, agent, tool)` in a sliding time window that
//! survives across turns (process-global registry; the fleet is a single
//! instance). Past `write_velocity_max_calls` inside
//! `write_velocity_window_secs`, the call is parked as a durable
//! pending-approval row — the same F-1 mechanism the irreversible tier uses
//! — so a human says ya/batal instead of the loop spending forever.
//!
//! Only calls that would otherwise execute (`Approved`, non-`Safe` tier)
//! are counted: denied, prompted, and already-pending calls flow through
//! their existing paths untouched.

use super::approval_gate::ApprovalGateOutcome;
use super::context::TurnCtx;
use crate::approval::ApprovalRequirement;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Process-global sliding-window counters. Keyed by
/// `(tenant_id, agent_type, tool_name)` so one tenant's bulk work never
/// throttles another, and one agent's loop never throttles its teammates.
///
/// Turns with no tenant context share the `("", "", tool)` bucket on
/// purpose: losing context narrows the budget (parks sooner), never widens
/// it. The map itself is bounded (see `MAX_REGISTRY_KEYS`): evicting a key
/// only resets that triple's count, which fails open toward availability
/// (a few more calls) rather than toward silent execution — and the
/// in-turn burst detector still bounds any single turn.
static REGISTRY: LazyLock<Mutex<HashMap<(String, String, String), VecDeque<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Upper bound on tracked triples. Hit only under adversarial or pathological
/// load (10k distinct tenant/agent/tool combos inside one window); normal
/// fleets track dozens.
const MAX_REGISTRY_KEYS: usize = 10_000;

/// Check a would-be tool call against the velocity gate.
///
/// Returns `Some(Deny)` carrying a freshly inserted pending-approval id when
/// the triple already hit its window budget (the caller must surface the
/// outcome exactly like a gate denial: emit the tool-call pair and
/// `continue`). Returns `None` when the call may proceed to the normal
/// approval gate.
pub(crate) fn check_velocity_park(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    iteration: usize,
) -> Option<ApprovalGateOutcome> {
    let max_calls = ctx.pacing.write_velocity_max_calls;
    if max_calls == 0 {
        return None;
    }
    let mgr = ctx.approval?;
    // Safe-tier tools (pure reads) are never velocity-gated.
    if mgr.risk_tier(tool_name) == zeroclaw_config::schema::ToolRiskTier::Safe {
        return None;
    }
    // Only gate calls that would actually execute. Anything the approval
    // gate would deny, prompt, or park already has its own handling — and
    // must not consume the execution budget.
    if mgr.approval_requirement(tool_name) != ApprovalRequirement::Approved {
        return None;
    }

    let tenant = crate::agent::tenant::current_tenant();
    let tenant_id = tenant
        .as_ref()
        .map(|t| t.tenant_id.clone())
        .unwrap_or_default();
    let agent_type = tenant
        .as_ref()
        .map(|t| t.agent_type.clone())
        .unwrap_or_default();
    let key = (tenant_id, agent_type, tool_name.to_string());

    let window = Duration::from_secs(ctx.pacing.write_velocity_window_secs.max(1));
    let now = Instant::now();
    let count = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        let len = {
            let entry = registry.entry(key).or_default();
            while entry
                .front()
                .is_some_and(|t| now.duration_since(*t) > window)
            {
                entry.pop_front();
            }
            entry.push_back(now);
            entry.len()
        };
        if registry.len() > MAX_REGISTRY_KEYS {
            // Shed load: drop fully-expired keys first, else one arbitrary
            // key. Eviction only ever resets a count (fail-open toward a few
            // more calls, never toward unlogged execution).
            registry.retain(|_, times| {
                times
                    .front()
                    .is_some_and(|t| now.duration_since(*t) <= window)
            });
            if registry.len() > MAX_REGISTRY_KEYS
                && let Some(victim) = registry.keys().next().cloned()
            {
                registry.remove(&victim);
            }
        }
        len
    };
    if count <= max_calls {
        return None;
    }

    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
            .with_category(::zeroclaw_log::EventCategory::Tool)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "model": ctx.model,
                "iteration": iteration + 1,
                "tool": tool_name,
                "calls_in_window": count,
                "window_secs": ctx.pacing.write_velocity_window_secs,
                "trace_id": ctx.turn_id,
            })),
        "write velocity budget exceeded — parking for human approval"
    );

    Some(park_as_pending(ctx, tool_name, tool_args, count))
}

/// Insert the parked call as a durable pending-approval row and shape the
/// denial exactly like the irreversible tier's Pending branch (same
/// `output_data.pending_id` contract the dashboard resolves), so the
/// conversational ya/batal path and the Approvals surface work unchanged.
fn park_as_pending(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    count: usize,
) -> ApprovalGateOutcome {
    use crate::agent::tool_execution::ToolExecutionOutcome;
    use std::time::Duration as StdDuration;

    let tenant = crate::agent::tenant::current_tenant();
    let principal = tenant
        .as_ref()
        .map(|t| t.platform_user_id.clone())
        .unwrap_or_default();
    let turn_origin = crate::agent::tenant::current_turn_origin();
    let tier_label = match ctx.approval.map(|mgr| mgr.risk_tier(tool_name)) {
        Some(zeroclaw_config::schema::ToolRiskTier::Irreversible) => "irreversible",
        _ => "reversible",
    };

    let store_opt = ctx.approval.and_then(|mgr| mgr.pending_store());
    let Some(store) = store_opt else {
        return deny_unrecorded(
            ctx,
            tool_name,
            count,
            "no pending-approval store is configured for this agent",
            false,
        );
    };
    let id = match store.insert_with_context(
        &principal,
        tool_name,
        &tool_args.to_string(),
        tier_label,
        tenant.as_ref().map(|t| t.tenant_id.as_str()),
        tenant.as_ref().map(|t| t.agent_type.as_str()),
        turn_origin.as_ref().and_then(|o| o.session_id.as_deref()),
        turn_origin.as_ref().map(|o| o.origin_message.as_str()),
        turn_origin.as_ref().and_then(|o| o.schedule_id.as_deref()),
    ) {
        Ok(id) => id,
        Err(e) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "tool": tool_name,
                        "error": format!("{e:#}"),
                        "trace_id": ctx.turn_id,
                    })),
                "velocity-park insert failed — the request was NOT recorded"
            );
            return deny_unrecorded(
                ctx,
                tool_name,
                count,
                "recording the pending-approval request failed",
                true,
            );
        }
    };
    // Read-your-write: never hand out an id the store cannot already serve
    // back (mirrors the gate's own guarantee).
    let pending_id: Option<String> = match store.get(&id) {
        Ok(Some(_)) => Some(id),
        Ok(None) | Err(_) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "tool": tool_name,
                        "pending_id": id,
                        "trace_id": ctx.turn_id,
                    })),
                "velocity-park insert not immediately readable — not surfacing the id"
            );
            None
        }
    };

    if let Some(id) = &pending_id {
        crate::agent::tenant::record_pending_approval(
            crate::agent::tenant::PendingApprovalSummary {
                id: id.clone(),
                tool_name: tool_name.to_string(),
                risk_tier: tier_label.to_string(),
            },
        );
    }

    let message = match &pending_id {
        Some(id) => format!(
            "Parked for human approval: '{tool_name}' already ran {count} times \
             in the recent window (possible loop — see pending_id={id}). \
             Say 'ya' to run it anyway, 'batal' to stop."
        ),
        // The row was inserted but the verification read failed: the id is
        // deliberately withheld (same rule as the gate — never surface an
        // unverifiable id), so report honestly instead of fake-parking.
        None => format!(
            "Not running '{tool_name}': it already ran {count} times in the \
             recent window (possible loop), and the pending-approval record \
             could not be confirmed — the request was NOT recorded and cannot \
             be resolved later."
        ),
    };
    if let Some(tx) = ctx.on_delta {
        let _ = tx.send(super::events::StreamDelta::Status(format!(
            "\u{23f8}\u{fe0f} {tool_name}: {message}\n"
        )));
    }
    ApprovalGateOutcome::Deny(ToolExecutionOutcome {
        output: message.clone(),
        success: false,
        error_reason: Some(message),
        duration: StdDuration::ZERO,
        receipt: None,
        output_data: pending_id.map(|id| serde_json::json!({"pending_id": id})),
    })
}

/// Deny without a pending row, with an honest reason: either no store is
/// configured, or recording failed. Never claims the call was parked.
fn deny_unrecorded(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    count: usize,
    reason: &str,
    log_error: bool,
) -> ApprovalGateOutcome {
    use crate::agent::tool_execution::ToolExecutionOutcome;
    use std::time::Duration as StdDuration;

    if log_error {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "tool": tool_name,
                    "trace_id": ctx.turn_id,
                })),
            "velocity-park could not be recorded"
        );
    }
    let message = format!(
        "Not running '{tool_name}': it already ran {count} times in the \
         recent window (possible loop), and {reason} — the request was NOT \
         recorded and cannot be resolved later."
    );
    if let Some(tx) = ctx.on_delta {
        let _ = tx.send(super::events::StreamDelta::Status(format!(
            "\u{23f8}\u{fe0f} {tool_name}: {message}\n"
        )));
    }
    ApprovalGateOutcome::Deny(ToolExecutionOutcome {
        output: message.clone(),
        success: false,
        error_reason: Some(message),
        duration: StdDuration::ZERO,
        receipt: None,
        output_data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tenant::{
        LAST_PENDING_APPROVAL, TENANT_CONTEXT, TURN_ORIGIN_CONTEXT, TenantContext,
        TurnOriginContext, take_pending_approval,
    };
    use crate::approval::ApprovalManager;
    use crate::control_plane::pending_approvals::PendingApprovalsStore;
    use crate::observability::NoopObserver;
    use crate::security::AutonomyLevel;
    use std::sync::Arc;
    use zeroclaw_config::schema::{PacingConfig, RiskProfileConfig, ToolRiskTiersConfig};

    fn test_pacing(max_calls: usize) -> PacingConfig {
        PacingConfig {
            write_velocity_window_secs: 600,
            write_velocity_max_calls: max_calls,
            ..PacingConfig::default()
        }
    }

    fn velocity_mgr(store: &Arc<PendingApprovalsStore>) -> ApprovalManager {
        let mut profile = RiskProfileConfig::default();
        profile.level = AutonomyLevel::Supervised;
        profile.auto_approve = ["create_lead".to_string()].into_iter().collect();
        let tiers = ToolRiskTiersConfig {
            irreversible: vec![],
            reversible: vec!["create_lead".to_string()],
        };
        ApprovalManager::for_non_interactive(&profile).with_risk_taxonomy(
            tiers,
            None,
            Some(Arc::clone(store)),
        )
    }

    fn velocity_ctx<'a>(
        observer: &'a NoopObserver,
        mgr: &'a ApprovalManager,
        pacing: &'a PacingConfig,
        tools: &'a Vec<String>,
    ) -> TurnCtx<'a> {
        TurnCtx {
            observer,
            provider_name: "test",
            model: "test-model",
            temperature: None,
            approval: Some(mgr),
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            event_tx: None,
            hooks: None,
            dedup_exempt_tools: tools,
            pacing,
            strict_tool_parsing: false,
            channel: None,
            draft_reasoning: zeroclaw_config::schema::StreamReasoningMode::Status,
            turn_id: "velocity-test-turn",
            agent_alias: None,
            parent_agent_alias: None,
        }
    }

    fn velocity_tenant(tag: &str) -> Arc<TenantContext> {
        Arc::new(TenantContext {
            tenant_id: format!("velocity-tenant-{tag}"),
            platform_user_id: format!("velocity-user-{tag}"),
            agent_type: "leads_qualifier".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: Vec::new(),
        })
    }

    #[tokio::test]
    async fn parks_after_budget_and_row_is_resolvable() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        let mgr = velocity_mgr(&store);
        let pacing = test_pacing(3);
        let observer = NoopObserver;
        let tools: Vec<String> = Vec::new();
        let ctx = velocity_ctx(&observer, &mgr, &pacing, &tools);
        let tenant = velocity_tenant("park");
        let turn_origin = Arc::new(TurnOriginContext {
            session_id: Some("sess-velocity".to_string()),
            origin_message: "log the lead".to_string(),
            schedule_id: None,
        });
        let pending_cell = Arc::new(parking_lot::Mutex::new(None));
        let args = serde_json::json!({"lead": "acme"});

        // First 3 approved calls pass through untouched; the 4th exceeds
        // the budget of 3 → parked with a resolvable row. All inside one
        // tenant scope so every call counts toward the same key.
        let (pending_id, summary_id) = TENANT_CONTEXT
            .scope(
                Some(tenant),
                TURN_ORIGIN_CONTEXT.scope(
                    Some(turn_origin),
                    LAST_PENDING_APPROVAL.scope(Some(pending_cell), async {
                        for _ in 0..3 {
                            assert!(check_velocity_park(&ctx, "create_lead", &args, 0).is_none());
                        }
                        let pending_id = match check_velocity_park(&ctx, "create_lead", &args, 0) {
                            Some(ApprovalGateOutcome::Deny(outcome)) => {
                                assert!(!outcome.success);
                                assert!(outcome.output.contains("Parked for human approval"));
                                outcome
                                    .output_data
                                    .as_ref()
                                    .and_then(|v| v.get("pending_id"))
                                    .and_then(|v| v.as_str())
                                    .expect("deny carries output_data.pending_id")
                                    .to_string()
                            }
                            other => panic!(
                                "expected velocity park, got {}",
                                if other.is_some() {
                                    "unexpected outcome"
                                } else {
                                    "None"
                                }
                            ),
                        };
                        // Still inside the scope: the summary is taken, not copied.
                        let summary = take_pending_approval().expect("summary recorded");
                        (pending_id, summary.id)
                    }),
                ),
            )
            .await;
        store
            .get(&pending_id)
            .expect("store readable")
            .expect("parked row exists");
        // The task-local summary handed to the response path names the same id.
        assert_eq!(summary_id, pending_id);
    }

    #[tokio::test]
    async fn safe_tools_never_park() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        let mut profile = RiskProfileConfig::default();
        profile.level = AutonomyLevel::Supervised;
        profile.auto_approve = ["memory_recall".to_string()].into_iter().collect();
        // Empty taxonomy so tier resolution runs and hits DEFAULT_SAFE_TOOLS
        // (with no taxonomy every tool reports Reversible).
        let tiers = ToolRiskTiersConfig {
            irreversible: vec![],
            reversible: vec![],
        };
        let mgr = ApprovalManager::for_non_interactive(&profile).with_risk_taxonomy(
            tiers,
            None,
            Some(Arc::clone(&store)),
        );
        let pacing = test_pacing(2);
        let observer = NoopObserver;
        let tools: Vec<String> = Vec::new();
        let ctx = velocity_ctx(&observer, &mgr, &pacing, &tools);
        let args = serde_json::json!({});

        for _ in 0..10 {
            assert!(
                check_velocity_park(&ctx, "memory_recall", &args, 0).is_none(),
                "Safe-tier reads must never consume the velocity budget"
            );
        }
    }

    #[tokio::test]
    async fn non_approved_calls_do_not_consume_budget() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        // create_lead NOT in auto_approve and tiered reversible → Prompt,
        // never Approved → never counted, never parked by this gate.
        let mut profile = RiskProfileConfig::default();
        profile.level = AutonomyLevel::Supervised;
        let tiers = ToolRiskTiersConfig {
            irreversible: vec![],
            reversible: vec!["create_lead".to_string()],
        };
        let mgr = ApprovalManager::for_non_interactive(&profile).with_risk_taxonomy(
            tiers,
            None,
            Some(Arc::clone(&store)),
        );
        let pacing = test_pacing(1);
        let observer = NoopObserver;
        let tools: Vec<String> = Vec::new();
        let ctx = velocity_ctx(&observer, &mgr, &pacing, &tools);
        let args = serde_json::json!({"lead": "acme"});

        for _ in 0..5 {
            assert!(check_velocity_park(&ctx, "create_lead", &args, 0).is_none());
        }
    }

    #[tokio::test]
    async fn disabled_at_zero() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        let mgr = velocity_mgr(&store);
        let pacing = test_pacing(0);
        let observer = NoopObserver;
        let tools: Vec<String> = Vec::new();
        let ctx = velocity_ctx(&observer, &mgr, &pacing, &tools);
        let args = serde_json::json!({"lead": "acme"});

        for _ in 0..20 {
            assert!(check_velocity_park(&ctx, "create_lead", &args, 0).is_none());
        }
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        let mgr = velocity_mgr(&store);
        let pacing = test_pacing(2);
        let observer = NoopObserver;
        let tools: Vec<String> = Vec::new();
        let ctx = velocity_ctx(&observer, &mgr, &pacing, &tools);
        let args = serde_json::json!({"lead": "acme"});

        // Tenant A burns its own budget…
        TENANT_CONTEXT
            .scope(Some(velocity_tenant("iso-a")), async {
                for _ in 0..2 {
                    assert!(check_velocity_park(&ctx, "create_lead", &args, 0).is_none());
                }
            })
            .await;
        // …tenant B is unaffected.
        TENANT_CONTEXT
            .scope(Some(velocity_tenant("iso-b")), async {
                for _ in 0..2 {
                    assert!(check_velocity_park(&ctx, "create_lead", &args, 0).is_none());
                }
            })
            .await;
    }
}
