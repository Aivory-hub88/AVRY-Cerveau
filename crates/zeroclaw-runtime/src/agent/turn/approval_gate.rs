//! The per-tool-call approval gate: CLI prompt, channel inline approval, or
//! auto-deny, plus decision recording.

use super::context::TurnCtx;
use super::events::StreamDelta;
use super::redact::scrub_credentials;
use crate::agent::tool_execution::ToolExecutionOutcome;
use crate::approval::{ApprovalRequest, ApprovalRequirement, ApprovalResponse};
use std::time::Duration;

pub(crate) enum ApprovalGateOutcome {
    Proceed { approved: bool },
    Deny(ToolExecutionOutcome),
    Replace(ToolExecutionOutcome),
}

/// Run the approval flow for one tool call (upstream loop body, approval
/// section): resolve the tool's approval requirement, prompt interactively on
/// CLI or via the channel's inline approval on non-interactive channels
/// (falling back to auto-deny), and record the decision.
pub(crate) async fn gate_tool_approval(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    iteration: usize,
) -> ApprovalGateOutcome {
    let mut approval_requirement = ctx
        .approval
        .map(|mgr| mgr.approval_requirement(tool_name))
        .unwrap_or(ApprovalRequirement::NotRequired);

    // Cerveau (enterprise-hardening round 1): an Irreversible-tier tool on
    // a non-interactive manager never executes and never falls through to
    // auto-deny — it creates a durable pending-approval record instead, so
    // a human can resolve it out-of-band later (see the `pending_approvals`
    // module doc for why this doesn't try to resume the original turn).
    if approval_requirement == ApprovalRequirement::Pending {
        let tenant = crate::agent::tenant::current_tenant();
        let principal = tenant
            .as_ref()
            .map(|t| t.platform_user_id.clone())
            .unwrap_or_default();
        let turn_origin = crate::agent::tenant::current_turn_origin();
        // Patch 0028: carry tenant/session/origin-message context on the
        // row whenever we have it, so a later tenant-scoped resolve call
        // can durably resume this turn instead of just executing the tool
        // out-of-band (see `pending_approvals`'s module doc). A row with no
        // tenant context (loopback/CLI-originated) still works exactly as
        // before — out-of-band execution only.
        let pending_id: Option<String> = match ctx.approval.and_then(|mgr| mgr.pending_store()) {
            Some(store) => {
                let id = match store.insert_with_context(
                    &principal,
                    tool_name,
                    &tool_args.to_string(),
                    "irreversible",
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
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "model": ctx.model,
                                "iteration": iteration + 1,
                                "tool": tool_name,
                                "error": format!("{e:#}"),
                                "trace_id": ctx.turn_id,
                            })),
                            "pending-approval insert failed — the request was NOT \
                             recorded and cannot be resolved later"
                        );
                        return ApprovalGateOutcome::Deny(ToolExecutionOutcome {
                            output: "Requires human approval before it can run (risk tier: irreversible), \
                                     but recording the pending-approval request failed — \
                                     the request was NOT recorded and cannot be resolved later."
                                .to_string(),
                            success: false,
                            error_reason: Some(
                                "pending-approval store insert failed".to_string(),
                            ),
                            duration: Duration::ZERO,
                            receipt: None,
                            output_data: None,
                        });
                    }
                };
                // Read-your-write guarantee: the id must already be durably
                // readable from this same store instance before anything
                // downstream (the `pending_approval` response field, the
                // reply text, the task-local summary) surfaces it. On the
                // current SQLite-backed store this is a single indexed PK
                // lookup on the same `Mutex<Connection>` that just committed
                // the insert, so it cannot fail spuriously — a miss here
                // means the store backend itself is broken (e.g. a future
                // pooled/remote backend serving a stale replica), and
                // handing the id out anyway would produce exactly the
                // "approve returns 404 for a real approval" symptom. Never
                // surface an unverifiable id.
                match store.get(&id) {
                    Ok(Some(_)) => Some(id),
                    Ok(None) | Err(_) => {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "model": ctx.model,
                                "iteration": iteration + 1,
                                "tool": tool_name,
                                "pending_id": id,
                                "trace_id": ctx.turn_id,
                            })),
                            "pending-approval insert not immediately readable — \
                             the request was NOT surfaced and cannot be resolved later"
                        );
                        return ApprovalGateOutcome::Deny(ToolExecutionOutcome {
                            output: "Requires human approval before it can run (risk tier: irreversible), \
                                     but the pending-approval record could not be confirmed — \
                                     the request was NOT recorded and cannot be resolved later."
                                .to_string(),
                            success: false,
                            error_reason: Some(
                                "pending-approval store verification read failed".to_string(),
                            ),
                            duration: Duration::ZERO,
                            receipt: None,
                            output_data: None,
                        });
                    }
                }
            }
            None => None,
        };
        if let Some(id) = &pending_id {
            // Cerveau (patch 0035): surface this structurally to whatever
            // scoped this turn (e.g. the webhook handler), so a channel
            // front-end can attach a real approve/deny affordance without
            // scraping the id back out of the model's own reply text — see
            // `PendingApprovalSummary`'s doc.
            crate::agent::tenant::record_pending_approval(
                crate::agent::tenant::PendingApprovalSummary {
                    id: id.clone(),
                    tool_name: tool_name.to_string(),
                    risk_tier: "irreversible".to_string(),
                },
            );
        }
        let message = match &pending_id {
            Some(id) => format!(
                "Requires human approval before it can run (risk tier: irreversible). \
                 Tracked as pending_id={id}; not yet executed."
            ),
            None => "Requires human approval before it can run (risk tier: irreversible), \
                     but no durable pending-approval store is configured for this agent — \
                     the request was NOT recorded and cannot be resolved later."
                .to_string(),
        };
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "model": ctx.model,
                    "iteration": iteration + 1,
                    "tool": tool_name,
                    "arguments": scrub_credentials(&tool_args.to_string()),
                    "pending_id": pending_id,
                    "trace_id": ctx.turn_id,
                })),
            "tool_call_result"
        );
        if let Some(tx) = ctx.on_delta {
            let _ = tx
                .send(StreamDelta::Status(format!(
                    "\u{23f8}\u{fe0f} {}: {}\n",
                    tool_name, message
                )))
                .await;
        }
        return ApprovalGateOutcome::Deny(ToolExecutionOutcome {
            output: message.clone(),
            success: false,
            error_reason: Some(message),
            duration: Duration::ZERO,
            receipt: None,
            output_data: pending_id.map(|id| serde_json::json!({"pending_id": id})),
        });
    }

    if let Some(mgr) = ctx.approval
        && approval_requirement == ApprovalRequirement::Prompt
    {
        let request = ApprovalRequest {
            tool_name: tool_name.to_string(),
            arguments: tool_args.clone(),
        };

        // Interactive CLI: prompt the operator.
        // Non-interactive (channels): try the channel's inline
        // approval (e.g. Telegram inline keyboard) before falling
        // back to auto-deny.
        let (decision, decided_by) = if mgr.is_non_interactive() {
            let attributed = if let Some(ch) = ctx.channel {
                let ch_request = zeroclaw_api::channel::ChannelApprovalRequest {
                    tool_name: request.tool_name.clone(),
                    arguments_summary: crate::approval::summarize_args(&request.arguments),
                    raw_arguments: Some(request.arguments.clone()),
                };
                let recipient = ctx.channel_reply_target.unwrap_or_default();
                match ch.request_approval_attributed(recipient, &ch_request).await {
                    Ok(Some(a)) => Some(a),
                    Ok(None) => None,
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Channel approval request failed"
                        );
                        None
                    }
                }
            } else {
                None
            };
            // The deciding back-channel (when a fan-out bridge answered) rides
            // back on the response itself, so attribution can't be cross-wired
            // by a concurrent approval on the same channel instance.
            let decided_by = attributed.as_ref().and_then(|a| a.decided_by.clone());
            let decision = match attributed.map(|a| a.response) {
                Some(zeroclaw_api::channel::ChannelApprovalResponse::Approve) => {
                    ApprovalResponse::Yes
                }
                Some(zeroclaw_api::channel::ChannelApprovalResponse::AlwaysApprove) => {
                    ApprovalResponse::Always
                }
                Some(zeroclaw_api::channel::ChannelApprovalResponse::Deny) => ApprovalResponse::No,
                Some(zeroclaw_api::channel::ChannelApprovalResponse::DenyWithEdit {
                    replacement,
                }) => ApprovalResponse::ReplaceWith(replacement),
                // Channel doesn't support approval — auto-deny.
                None => ApprovalResponse::No,
            };
            (decision, decided_by)
        } else {
            (mgr.prompt_cli(&request), None)
        };

        let decision_channel = decided_by.unwrap_or_else(|| ctx.channel_name.to_string());
        mgr.record_decision(tool_name, tool_args, &decision, &decision_channel);

        if decision == ApprovalResponse::No {
            let denied = "Denied by user.".to_string();
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": ctx.model,
                        "iteration": iteration + 1,
                        "tool": tool_name,
                        "arguments": scrub_credentials(&tool_args.to_string()),
                        "result": denied,
                        "trace_id": ctx.turn_id,
                    })),
                "tool_call_result"
            );
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{274c} {}: {}\n",
                        tool_name, denied
                    )))
                    .await;
            }
            return ApprovalGateOutcome::Deny(ToolExecutionOutcome {
                output: denied.clone(),
                success: false,
                error_reason: Some(denied),
                duration: Duration::ZERO,
                receipt: None,
                output_data: None,
            });
        }

        if let ApprovalResponse::ReplaceWith(replacement) = &decision {
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{270f} {}: replaced by user\n",
                        tool_name
                    )))
                    .await;
            }
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Approve)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "model": ctx.model,
                        "iteration": iteration + 1,
                        "tool": tool_name,
                        "arguments": scrub_credentials(&tool_args.to_string()),
                        "replaced": true,
                        "output": scrub_credentials(replacement),
                        "trace_id": ctx.turn_id,
                    })),
                "tool_call_result"
            );
            return ApprovalGateOutcome::Replace(ToolExecutionOutcome {
                output: crate::approval::sanitize_tool_replacement(replacement),
                success: true,
                error_reason: None,
                duration: Duration::ZERO,
                receipt: None,
                output_data: None,
            });
        }

        if matches!(decision, ApprovalResponse::Yes | ApprovalResponse::Always) {
            approval_requirement = ApprovalRequirement::Approved;
        }
    }

    ApprovalGateOutcome::Proceed {
        approved: approval_requirement == ApprovalRequirement::Approved,
    }
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
    use zeroclaw_config::schema::{RiskProfileConfig, ToolRiskTiersConfig};

    /// The read-your-write guarantee: an approval id produced by the exact
    /// code path a real tool-call-needing-approval takes
    /// (`gate_tool_approval` → `insert_with_context` → task-local summary →
    /// response `output_data`) must already be durably readable via
    /// `store.get(&id)` the instant it is handed out — never 404 on first
    /// contact. Regression test for the 2026-09-17 production incident
    /// where Approve clicked the instant a card appeared returned
    /// `no pending approval with id ...` (that incident turned out to be
    /// proxy fan-out 404s plus a 53s synchronous continuation, not a store
    /// race — this test pins the store side regardless, so a future
    /// backend swap can never reintroduce it silently).
    #[tokio::test]
    async fn gated_tool_approval_id_is_immediately_readable() {
        let store = Arc::new(PendingApprovalsStore::new_in_memory().unwrap());
        let profile = RiskProfileConfig {
            level: AutonomyLevel::Supervised,
            ..RiskProfileConfig::default()
        };
        let tiers = ToolRiskTiersConfig {
            irreversible: vec!["finalize_invoice".to_string()],
            reversible: vec![],
        };
        let mgr = ApprovalManager::for_non_interactive(&profile).with_risk_taxonomy(
            tiers,
            None,
            Some(Arc::clone(&store)),
        );

        let tenant = Arc::new(TenantContext {
            tenant_id: "u1.leads_qualifier".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "leads_qualifier".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: Vec::new(),
        });
        let turn_origin = Arc::new(TurnOriginContext {
            session_id: Some("sess-1".to_string()),
            origin_message: "please finalize invoice inv_123".to_string(),
            schedule_id: None,
        });
        let pending_cell = Arc::new(parking_lot::Mutex::new(None));

        let observer = NoopObserver;
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let empty_tools: Vec<String> = Vec::new();
        let ctx = TurnCtx {
            observer: &observer,
            provider_name: "test",
            model: "test-model",
            temperature: None,
            approval: Some(&mgr),
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            event_tx: None,
            hooks: None,
            dedup_exempt_tools: &empty_tools,
            pacing: &pacing,
            strict_tool_parsing: false,
            channel: None,
            turn_id: "test-turn",
            agent_alias: None,
            parent_agent_alias: None,
        };

        let tool_args = serde_json::json!({"invoice_id": "inv_123"});
        let (pending_id, summary_id) = TENANT_CONTEXT
            .scope(
                Some(tenant),
                TURN_ORIGIN_CONTEXT.scope(
                    Some(turn_origin),
                    LAST_PENDING_APPROVAL.scope(Some(pending_cell), async {
                        let pending_id = match gate_tool_approval(
                            &ctx,
                            "finalize_invoice",
                            &tool_args,
                            0,
                        )
                        .await
                        {
                            ApprovalGateOutcome::Deny(outcome) => {
                                assert!(!outcome.success);
                                outcome
                                    .output_data
                                    .as_ref()
                                    .and_then(|v| v.get("pending_id"))
                                    .and_then(|v| v.as_str())
                                    .expect("deny carries output_data.pending_id")
                                    .to_string()
                            }
                            other => panic!(
                                "irreversible tool on non-interactive must deny-pending, got {}",
                                match other {
                                    ApprovalGateOutcome::Proceed { .. } => "Proceed",
                                    ApprovalGateOutcome::Deny(_) => "Deny",
                                    ApprovalGateOutcome::Replace(_) => "Replace",
                                }
                            ),
                        };
                        // Still inside the scope: this is the only place the
                        // summary is readable (it is taken, not copied).
                        let summary = take_pending_approval().expect("summary recorded");
                        (pending_id, summary.id)
                    }),
                ),
            )
            .await;

        // The task-local summary handed to the response path names the same id.
        assert_eq!(summary_id, pending_id);

        // And that id is durably readable *right now* — this is the
        // invariant the resolve endpoint depends on.
        let row = store.get(&pending_id).unwrap().expect("row readable");
        assert_eq!(row.status, "pending");
        assert_eq!(row.tool_name, "finalize_invoice");
        assert_eq!(row.tenant_id.as_deref(), Some("u1.leads_qualifier"));
        assert_eq!(row.agent_type.as_deref(), Some("leads_qualifier"));
    }
}
