//! Cerveau (F-1-for-approvals, patch 0029): tenant-scoped resolve of a
//! `Pending` approval that also resumes the original conversation, instead
//! of only executing the tool out-of-band
//! (`api_approvals::execute_approved_tool`, the loopback-only path this
//! module reuses for the actual tool call — nothing about tool execution
//! changes here, only what happens after).
//!
//! There is no literal saved transcript to resume into —
//! `agent::process_message` rebuilds `history` from scratch on every call,
//! and a webhook-driven tenant turn's continuity comes only from memory
//! recall scoped by `session_id` (see
//! `zeroclaw_runtime::agent::tenant::TurnOriginContext`'s doc). So "resume"
//! here means: re-resolve the tenant's persona/connected-toolkits fresh
//! (exactly as a live `/webhook` call would), then run ONE continuation
//! turn against a synthesized prompt that tells the model what it asked,
//! what it called, and what happened — mirroring
//! `control_plane::continuation_drive`'s established "system continuation
//! prompt + one more real turn" shape (patch 0025), adapted for a tenant
//! turn instead of a crashed goal task.
//!
//! Reachable via `POST /webhook/approvals/{id}/resolve`
//! ([`handle_webhook_approval_resolve`], patch 0030) — the same
//! `X-Webhook-Secret` contract `/webhook` already uses, plus a row-
//! ownership check (the row's stored `tenant_id` must match the caller's
//! `X-Tenant-Id`/`X-Agent-Type`) since a valid shared secret alone proves
//! only "this is the bridge," not "this is the tenant that owns this row".

use std::sync::Arc;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_runtime::control_plane::pending_approvals::PendingApproval;
use zeroclaw_runtime::security::pairing::constant_time_eq;

use crate::AppState;
use crate::tenant::{TenantResolver, TenantSelector, ToolkitConnectionResolver, build_tenant_context};

type JsonErr = (StatusCode, Json<serde_json::Value>);

/// Outcome of resolving one tenant-scoped pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantResolveOutcome {
    /// `"approved"` | `"denied"` | `"already_resolved"`.
    pub outcome: &'static str,
    /// The continuation turn's reply, when one ran. `None` on
    /// `already_resolved` (nothing new happened this call) or when the
    /// continuation turn itself failed — the decision (and, on approve,
    /// the real tool side effect) is never lost even when this is `None`;
    /// see `run_continuation`'s doc for why a reply failure is logged and
    /// swallowed here rather than propagated.
    pub reply_text: Option<String>,
}

#[derive(Deserialize)]
pub struct ResolveBody {
    /// `"approve"` | `"deny"`.
    decision: String,
}

/// `POST /webhook/approvals/{id}/resolve` — tenant-scoped, patch 0030.
///
/// Auth is two layers, both required: (1) the same `X-Webhook-Secret`
/// check `handle_webhook` already does — proves the caller is genuinely
/// the bridge, not an arbitrary internet request; (2) the row's own
/// `tenant_id` must match the caller's `X-Tenant-Id`/`X-Agent-Type` —
/// proves the caller is resolving a row that actually belongs to the
/// tenant it claims to be, since a valid shared secret alone proves
/// nothing about *which* tenant is calling. A mismatch (or an id that
/// simply doesn't exist) both return 404, not 403 — this endpoint never
/// confirms whether a given pending-approval id exists to a caller who
/// doesn't already own it.
pub async fn handle_webhook_approval_resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<impl IntoResponse, JsonErr> {
    fn not_found(id: &str) -> JsonErr {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no pending approval with id {id}") })),
        )
    }
    fn unauthorized(msg: &str) -> JsonErr {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": msg })))
    }
    fn unavailable(msg: String) -> JsonErr {
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": msg })))
    }

    // ── Layer 1: prove this is really the bridge ──
    let Some(ref secret_hash) = state.webhook_secret_hash else {
        return Err(unauthorized(
            "tenant-scoped approval resolution requires X-Webhook-Secret auth on this deployment",
        ));
    };
    let header_hash = headers
        .get("X-Webhook-Secret")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::hash_webhook_secret);
    match header_hash {
        Some(val) if constant_time_eq(&val, secret_hash.as_ref()) => {}
        _ => return Err(unauthorized("invalid or missing X-Webhook-Secret header")),
    }

    // ── Layer 2: prove this is really the tenant that owns this row ──
    let sel = match crate::tenant::TenantSelector::from_headers(&headers) {
        Ok(Some(sel)) => sel,
        Ok(None) => {
            return Err(unauthorized(
                "X-Tenant-Id and X-Agent-Type are required to resolve a pending approval",
            ));
        }
        Err(reason) => return Err(unauthorized(reason)),
    };

    let data_dir = state.config.read().data_dir.clone();
    let store = zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore::shared(&data_dir)
        .map_err(|e| unavailable(format!("pending-approval store unavailable: {e}")))?;
    let row = store
        .get(&id)
        .map_err(|e| unavailable(format!("lookup failed: {e}")))?
        .ok_or_else(|| not_found(&id))?;

    // Row ownership, not the secret, is the actual authorization boundary
    // here — see this handler's own doc comment.
    if row.tenant_id.as_deref() != Some(sel.tenant_id().as_str()) {
        return Err(not_found(&id));
    }

    match resolve_and_continue_tenant_approval(&state, &row, &body.decision, "tenant-webhook").await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "outcome": outcome.outcome,
                "id": id,
                "reply_text": outcome.reply_text,
            })),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// Resolve a tenant-scoped pending approval and, unless it was already
/// resolved, run one continuation turn so the model can react to the real
/// outcome (the tool's actual result on approve, a plain decline on deny)
/// and produce a reply — the "seamless resume" this patch series exists
/// to add. `resolved_by` is an audit-trail label only (e.g. a channel/bot
/// identity), never an authorization check — the caller (patch 0030's
/// route) is responsible for verifying the request is genuinely scoped to
/// this row's own tenant before calling this.
pub(crate) async fn resolve_and_continue_tenant_approval(
    state: &AppState,
    row: &PendingApproval,
    decision: &str,
    resolved_by: &str,
) -> anyhow::Result<TenantResolveOutcome> {
    let data_dir = state.config.read().data_dir.clone();
    let store =
        zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore::shared(&data_dir)?;

    let (status, verb) = match decision {
        "approve" => ("approved", "approved"),
        "deny" => ("denied", "denied"),
        other => anyhow::bail!("decision must be \"approve\" or \"deny\", got \"{other}\""),
    };

    // Same idempotency contract as the loopback admin path
    // (`api_approvals::handle_resolve_approval`): a second resolve call for
    // an already-non-pending row is a no-op, not a re-execution and not a
    // second continuation turn.
    if !store.resolve(&row.id, status, resolved_by)? {
        return Ok(TenantResolveOutcome {
            outcome: "already_resolved",
            reply_text: None,
        });
    }

    // The decision is durably recorded above regardless of what happens
    // next, same posture as the loopback path: a downstream failure (tool
    // execution, or the continuation turn itself) must never un-record a
    // human's real "yes, do this" decision.
    let tool_result: Option<serde_json::Value> = if decision == "approve" {
        Some(crate::api_approvals::execute_approved_tool(state, row).await)
    } else {
        None
    };

    let prompt = continuation_prompt(row, decision, tool_result.as_ref());
    let reply_text = match run_continuation(state, row, prompt).await {
        Ok(text) => {
            // Patch 0032: mark delivered now that the reply is actually
            // about to be handed back in this response — this is what
            // makes an interrupted-before-here row (daemon restart between
            // the line above and the caller receiving these bytes) a
            // genuine reaper candidate, and a normally-completed one not.
            if let Err(e) = store.mark_delivered(&row.id) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "pending_id": row.id,
                            "error": format!("{e:#}"),
                        })),
                    "approval resume: mark_delivered failed after a successful reply — the \
                     reaper may attempt a harmless redundant redelivery for this row later"
                );
            }
            Some(text)
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "pending_id": row.id,
                        "error": format!("{e:#}"),
                    })),
                "approval resume: continuation turn failed after the decision was already \
                 durably recorded — the decision (and, on approve, the real tool side effect) \
                 stands; only the reply is missing this round. A future reaper sweep (patch \
                 0032) is the planned redelivery path for this case."
            );
            None
        }
    };

    Ok(TenantResolveOutcome {
        outcome: verb,
        reply_text,
    })
}

/// Synthesize the continuation prompt — the F-1-for-goals pattern
/// (`continuation_drive::drive_claimed`'s `prompt` local) adapted for an
/// approval resume: tell the model what the user originally asked, what it
/// tried to call, and what actually happened, so it can produce a coherent
/// reply instead of restating the request from nothing.
fn continuation_prompt(
    row: &PendingApproval,
    decision: &str,
    tool_result: Option<&serde_json::Value>,
) -> String {
    let origin = row
        .origin_message
        .as_deref()
        .unwrap_or("(original message not captured)");
    match (decision, tool_result) {
        ("approve", Some(result)) => format!(
            "[approval-resume:{}] The user asked: \"{origin}\". You called {} with arguments \
             {}, which required human approval before it could run. The user approved it. \
             Result: {result}. Continue your response to the user now, incorporating this \
             result.",
            row.id, row.tool_name, row.arguments
        ),
        _ => format!(
            "[approval-resume:{}] The user asked: \"{origin}\". You called {} with arguments \
             {}, which required human approval before it could run. The user declined to \
             approve it. Acknowledge this to the user and continue your response without \
             having run that action.",
            row.id, row.tool_name, row.arguments
        ),
    }
}

/// Re-resolve fresh tenant context (persona, connected toolkits) exactly as
/// a live `/webhook` call would (`handle_webhook`'s own tenant-resolution
/// block), then run one continuation turn scoped to it. Errors here (no
/// `agent_type`/`principal` on the row, persona-resolution failure, no
/// configured agent alias, the turn itself erroring) all propagate — the
/// caller logs and swallows them, per this function's own doc on why a
/// reply failure doesn't undo the already-recorded decision.
/// Pure precondition check, pulled out of `run_continuation` so it's
/// testable without an `AppState`: a row must carry `agent_type` and a
/// non-empty `principal` to be resumable at all — neither the tenant
/// resolver nor an LLM call should ever be reached for a row that can't
/// clear this (e.g. one inserted via the plain `insert()`, a loopback/CLI
/// caller with no tenant context).
fn tenant_selector_for_resume(row: &PendingApproval) -> anyhow::Result<TenantSelector> {
    let agent_type = row.agent_type.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "row has no agent_type — cannot resume (predates patch 0028's context columns, \
             or was created by a non-tenant/loopback caller)"
        )
    })?;
    anyhow::ensure!(!row.principal.is_empty(), "row has no principal — cannot resume");
    Ok(TenantSelector {
        user_id: row.principal.clone(),
        agent_type: agent_type.to_string(),
    })
}

async fn run_continuation(state: &AppState, row: &PendingApproval, prompt: String) -> anyhow::Result<String> {
    let sel = tenant_selector_for_resume(row)?;

    let persona = TenantResolver::global().resolve(&sel).await?;
    let connected_toolkits = ToolkitConnectionResolver::global().resolve(&sel.user_id).await;
    let tenant_ctx = build_tenant_context(&sel, persona.as_deref(), connected_toolkits);

    let turn_origin = Arc::new(zeroclaw_runtime::agent::tenant::TurnOriginContext {
        session_id: row.session_id.clone(),
        origin_message: row.origin_message.clone().unwrap_or_default(),
    });

    let config = state.config.read().clone();
    // No `agent_override`: a pending-approval row carries `agent_type`
    // (the Aivory persona axis), never a `?agent=` host-alias override —
    // Aivory tenant traffic never passes one either (see
    // `TenantContext::agent_type`'s own doc for why these are separate
    // axes), so the original turn resolved its host alias the exact same
    // way this resumes it.
    //
    // `resolve_gateway_chat_agent_alias` (not the `require_*` wrapper,
    // which is `#[cfg(not(test))]`-gated because `run_gateway_chat_with_
    // tools`'s own test branch never reaches it) — mapped to an error by
    // hand so this function compiles and behaves the same under both cfgs.
    let agent_alias = crate::resolve_gateway_chat_agent_alias(&config, None).ok_or_else(|| {
        anyhow::anyhow!("no configured [agents.<alias>] entry to resume this approval on")
    })?;

    let cost_tracking_context = state.cost_tracker.as_ref().map(|tracker| {
        let pricing = zeroclaw_runtime::agent::cost::build_model_provider_pricing(&config);
        zeroclaw_runtime::agent::cost::ToolLoopCostTrackingContext::new(tracker.clone(), Arc::new(pricing))
            .with_agent_alias(&agent_alias)
    });

    Box::pin(zeroclaw_runtime::agent::tenant::TENANT_CONTEXT.scope(
        Some(tenant_ctx),
        zeroclaw_runtime::agent::tenant::TURN_ORIGIN_CONTEXT.scope(
            Some(turn_origin),
            zeroclaw_runtime::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                cost_tracking_context,
                zeroclaw_runtime::agent::process_message(
                    config,
                    &agent_alias,
                    &prompt,
                    row.session_id.as_deref(),
                    TurnOrigin::Daemon,
                ),
            ),
        ),
    ))
    .await
}

/// Cerveau (patch 0031): best-effort outbound redelivery of an approval-
/// resume reply. Not called from the happy path — `handle_webhook_approval_
/// resolve` already returns `reply_text` directly in its own HTTP response.
/// This exists solely for the future reaper sweep (patch 0032) to re-drive
/// delivery for a row that was approved (and, on approve, whose tool side
/// effect already ran — F-2-protected, unaffected by this call succeeding
/// or failing) but whose reply was never delivered, e.g. because the
/// daemon restarted between F-2 `complete()` and the resolve response
/// reaching its caller.
///
/// Fire-and-forget: returns `Err` on any failure (no URL configured,
/// unreachable endpoint, non-2xx, timeout) rather than retrying — retrying
/// a failed redelivery is the reaper's own job on its next sweep, not
/// this function's.
pub(crate) async fn redeliver_resume_reply(
    state: &AppState,
    row: &PendingApproval,
    reply_text: &str,
) -> anyhow::Result<()> {
    let (url, secret) = {
        let config = state.config.read();
        (
            config.gateway.approval_redelivery_url.clone(),
            config.gateway.approval_redelivery_secret.clone(),
        )
    };
    let Some(url) = url else {
        anyhow::bail!("no gateway.approval_redelivery_url configured — redelivery skipped");
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build redelivery HTTP client")?;
    let mut req = client.post(&url).json(&serde_json::json!({
        "pending_id": row.id,
        "tenant_id": row.tenant_id,
        "agent_type": row.agent_type,
        "principal": row.principal,
        "session_id": row.session_id,
        "reply_text": reply_text,
    }));
    if let Some(secret) = secret {
        req = req.header("X-Redelivery-Secret", secret);
    }

    let resp = req.send().await.context("redelivery POST failed")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "redelivery endpoint returned {}",
        resp.status()
    );
    Ok(())
}

/// Cerveau (patch 0032): a resolved row is skipped from this sweep until
/// it's been sitting undelivered for at least this long — a row resolved
/// moments ago is far more likely a live, still-in-flight synchronous
/// resolve call than a genuinely stuck one, and racing it would only waste
/// a redundant continuation turn (harmless, since `mark_delivered` is
/// idempotent, but still pure waste).
const REDELIVERY_MIN_AGE: chrono::Duration = chrono::Duration::minutes(2);

/// Cerveau (patch 0032): best-effort periodic sweep for resolved-but-
/// undelivered approval rows — the restart/redelivery gap this patch
/// series' own design doc flagged (a daemon restart between a continuation
/// turn completing and its reply reaching the caller in
/// `resolve_and_continue_tenant_approval`'s own response). Mirrors
/// `control_plane::continuation_drive`'s "log, never fail the daemon"
/// posture for background supervision work — one candidate's failure never
/// stops the rest, and nothing here is a fatal error.
pub(crate) async fn sweep_undelivered_approvals(state: &AppState) {
    let data_dir = state.config.read().data_dir.clone();
    let store = match zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore::shared(
        &data_dir,
    ) {
        Ok(s) => s,
        Err(e) => {
            log_sweep_note("n/a", "open_store_failed", &format!("{e:#}"));
            return;
        }
    };
    let candidates = match store.list_undelivered_resolved() {
        Ok(rows) => rows,
        Err(e) => {
            log_sweep_note("n/a", "list_failed", &format!("{e:#}"));
            return;
        }
    };

    let now = chrono::Utc::now();
    for row in candidates {
        let old_enough = row
            .resolved_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|resolved_at| now.signed_duration_since(resolved_at) >= REDELIVERY_MIN_AGE);
        if !old_enough {
            continue;
        }
        redeliver_one_candidate(state, &store, &row).await;
    }
}

async fn redeliver_one_candidate(
    state: &AppState,
    store: &zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore,
    row: &PendingApproval,
) {
    let tool_result = if row.status == "approved" {
        match cached_tool_result(state, row) {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                // The F-2 ledger has no record at all for this row's tool
                // call — it never actually ran (the crash happened before
                // execution even started). Re-executing an irreversible
                // action unattended, from a background sweep with nobody
                // immediately able to notice a problem, is a materially
                // different risk than redelivering a reply for work that's
                // already done — deliberately NOT attempted here. Left for
                // operator visibility instead.
                log_sweep_note(
                    &row.id,
                    "tool_never_executed",
                    "F-2 ledger has no entry for this row's tool call — the decision was \
                     recorded but the action itself never ran; not auto-executing it from a \
                     background sweep, needs operator attention",
                );
                return;
            }
            Err(e) => {
                log_sweep_note(&row.id, "ledger_check_failed", &format!("{e:#}"));
                return;
            }
        }
    } else {
        None
    };

    let decision = if row.status == "approved" { "approve" } else { "deny" };
    let prompt = continuation_prompt(row, decision, tool_result.as_ref());
    let reply_text = match run_continuation(state, row, prompt).await {
        Ok(text) => text,
        Err(e) => {
            log_sweep_note(&row.id, "continuation_failed", &format!("{e:#}"));
            return;
        }
    };

    if let Err(e) = redeliver_resume_reply(state, row, &reply_text).await {
        log_sweep_note(&row.id, "redelivery_failed", &format!("{e:#}"));
        return;
    }

    if let Err(e) = store.mark_delivered(&row.id) {
        log_sweep_note(&row.id, "mark_delivered_failed", &format!("{e:#}"));
    }
}

/// Read-only lookup of a prior tool execution's cached result for an
/// `approved` row, via [`zeroclaw_runtime::control_plane::tool_idem::ToolIdemLedger::status`]
/// — deliberately the non-mutating lookup, not `claim()`: this function
/// must never itself claim (and thereby block any future legitimate
/// attempt at) a tool call that hasn't actually run yet. `Ok(None)` means
/// no attempt was ever recorded; `Ok(Some(_))` always carries a result
/// (an `InFlight` status is treated the same as "not safely reusable" —
/// see the call site).
fn cached_tool_result(state: &AppState, row: &PendingApproval) -> anyhow::Result<Option<serde_json::Value>> {
    let config = state.config.read();
    let principal = (!row.principal.is_empty()).then_some(row.principal.as_str());
    let idem_key = zeroclaw_runtime::control_plane::tool_idem::derive_key(
        principal.unwrap_or(""),
        &row.id,
        &row.id,
        &row.tool_name,
        &row.arguments,
    );
    let ledger = zeroclaw_runtime::control_plane::tool_idem::ToolIdemLedger::shared(&config.data_dir)?;
    Ok(match ledger.status(&idem_key)? {
        Some(zeroclaw_runtime::control_plane::tool_idem::Claim::AlreadyDone(output)) => {
            Some(serde_json::json!({ "success": true, "output": output }))
        }
        // InFlight (claimed but never completed) or no record at all are
        // both "not safely reusable" from this read-only vantage point —
        // neither proves the tool's real side effect happened.
        Some(zeroclaw_runtime::control_plane::tool_idem::Claim::InFlight) | None => None,
        Some(zeroclaw_runtime::control_plane::tool_idem::Claim::Claimed) => {
            // `status()` never returns `Claimed` (see its own doc — that
            // variant is only ever produced by `claim()`'s insert path);
            // unreachable in practice, handled for exhaustiveness only.
            None
        }
    })
}

fn log_sweep_note(pending_id: &str, kind: &str, detail: &str) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "pending_id": pending_id,
                "kind": kind,
                "detail": detail,
            })),
        "approval redelivery sweep: notable event"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No real LLM call — proves the decision short-circuits correctly
    /// before `run_continuation` is ever reached, per this fork's "don't
    /// spend real LLM cost in tests" convention (mirrors
    /// `continuation_drive`'s own `a_candidate_that_cannot_run_is_re_
    /// parked_not_left_running` test in spirit: prove the safe-degradation
    /// contract without a real turn).
    #[test]
    fn continuation_prompt_approve_names_the_result() {
        let row = PendingApproval {
            id: "pa_1".into(),
            principal: "u1".into(),
            tool_name: "zendesk__reply_ticket".into(),
            arguments: "{\"id\":42}".into(),
            risk_tier: "irreversible".into(),
            requested_at: "2026-01-01T00:00:00Z".into(),
            status: "approved".into(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: Some("u1.customer_service".into()),
            agent_type: Some("customer_service".into()),
            session_id: Some("sess-1".into()),
            origin_message: Some("please reply to ticket 42".into()),
            delivered_at: None,
        };
        let result = serde_json::json!({"success": true, "output": "ok"});
        let prompt = continuation_prompt(&row, "approve", Some(&result));
        assert!(prompt.contains("please reply to ticket 42"));
        assert!(prompt.contains("zendesk__reply_ticket"));
        assert!(prompt.contains("The user approved it"));
        assert!(prompt.contains("\"success\":true"));
    }

    #[test]
    fn continuation_prompt_deny_never_mentions_a_result() {
        let row = PendingApproval {
            id: "pa_2".into(),
            principal: "u1".into(),
            tool_name: "zendesk__reply_ticket".into(),
            arguments: "{}".into(),
            risk_tier: "irreversible".into(),
            requested_at: "2026-01-01T00:00:00Z".into(),
            status: "denied".into(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: None,
            agent_type: None,
            session_id: None,
            origin_message: None,
            delivered_at: None,
        };
        let prompt = continuation_prompt(&row, "deny", None);
        assert!(prompt.contains("declined to approve"));
        assert!(prompt.contains("(original message not captured)"));
        assert!(!prompt.contains("Result:"));
    }

    #[test]
    fn tenant_selector_for_resume_rejects_a_row_with_no_agent_type() {
        let row = PendingApproval {
            id: "pa_3".into(),
            principal: "u1".into(),
            tool_name: "t".into(),
            arguments: "{}".into(),
            risk_tier: "irreversible".into(),
            requested_at: "2026-01-01T00:00:00Z".into(),
            status: "approved".into(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: None,
            // No agent_type — e.g. a row inserted via the plain `insert()`
            // (loopback/CLI caller). Must fail fast, before ever touching
            // the tenant resolver or attempting an LLM call.
            agent_type: None,
            session_id: None,
            origin_message: None,
            delivered_at: None,
        };
        let err = tenant_selector_for_resume(&row).unwrap_err();
        assert!(err.to_string().contains("no agent_type"));
    }

    #[test]
    fn tenant_selector_for_resume_rejects_an_empty_principal() {
        let row = PendingApproval {
            id: "pa_4".into(),
            principal: String::new(),
            tool_name: "t".into(),
            arguments: "{}".into(),
            risk_tier: "irreversible".into(),
            requested_at: "2026-01-01T00:00:00Z".into(),
            status: "approved".into(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: None,
            agent_type: Some("customer_service".into()),
            session_id: None,
            origin_message: None,
            delivered_at: None,
        };
        let err = tenant_selector_for_resume(&row).unwrap_err();
        assert!(err.to_string().contains("no principal"));
    }

    #[test]
    fn tenant_selector_for_resume_accepts_a_well_formed_row() {
        let row = PendingApproval {
            id: "pa_5".into(),
            principal: "u1".into(),
            tool_name: "t".into(),
            arguments: "{}".into(),
            risk_tier: "irreversible".into(),
            requested_at: "2026-01-01T00:00:00Z".into(),
            status: "approved".into(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: Some("u1.customer_service".into()),
            agent_type: Some("customer_service".into()),
            session_id: Some("sess-1".into()),
            origin_message: Some("hi".into()),
            delivered_at: None,
        };
        let sel = tenant_selector_for_resume(&row).unwrap();
        assert_eq!(sel.user_id, "u1");
        assert_eq!(sel.agent_type, "customer_service");
    }
}
