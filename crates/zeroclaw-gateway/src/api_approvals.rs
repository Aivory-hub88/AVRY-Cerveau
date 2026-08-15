//! Cerveau (enterprise-hardening round 1): out-of-band surface for
//! `Irreversible`-tier tool calls that resolved
//! `ApprovalRequirement::Pending` — `GET /api/approvals`,
//! `POST /api/approvals/{id}/resolve`.
//!
//! Auth mirrors `api_sop.rs`'s admin gate verbatim (loopback CLI allowed;
//! remote needs `gateway.allow_remote_admin` + pairing + auth) — this
//! surface can trigger a real Stripe charge or Word-document write, the
//! same bar as SOP's out-of-band approval, not a lower one.
//!
//! Approving a row executes the tool directly, out-of-band, from this
//! handler — it does NOT resume the original conversational turn (that
//! needs Cerveau's still-unbuilt F-1 auto-resume driver, ADR-003). See the
//! `pending_approvals` module doc in `zeroclaw-runtime` for the full
//! rationale; this is a documented, intentional simplification for this
//! round, not an oversight.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::{AdminReloadGate, AppState, admin_reload_gate};
use zeroclaw_config::schema::apply_tenant_entity_scoping;

type JsonErr = (StatusCode, Json<serde_json::Value>);

fn disabled(reason: &str) -> JsonErr {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": reason })),
    )
}

/// Authorize a pending-approval call, returning an audit-trail label for
/// `resolved_by`. Mirrors `api_sop::authorize` (which returns a typed
/// `ApprovalPrincipal` for the same purpose) — kept as a separate copy
/// rather than a shared helper because `api_sop.rs` itself already
/// duplicates this from `handle_admin_reload` (see that file's own doc
/// comment); this fork's convention is each admin-style surface stays
/// self-contained here, not layered through a shared indirection.
fn authorize(
    state: &AppState,
    peer: &SocketAddr,
    headers: &HeaderMap,
) -> Result<&'static str, JsonErr> {
    let allow_remote = state.config.read().gateway.allow_remote_admin;
    let require_pairing = state.pairing.require_pairing();
    match admin_reload_gate(peer.ip().is_loopback(), allow_remote, require_pairing) {
        AdminReloadGate::Allow => Ok("loopback-cli"),
        AdminReloadGate::RequireAuth => {
            crate::api::require_auth(state, headers)?;
            Ok("authenticated-remote")
        }
        AdminReloadGate::Forbidden | AdminReloadGate::ForbiddenNoPairing => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "Remote pending-approval access is disabled. Call from localhost, \
                          or set gateway.allow_remote_admin = true with pairing enabled, \
                          then pair."
            })),
        )),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
}

fn pending_approval_json(
    row: &zeroclaw_runtime::control_plane::pending_approvals::PendingApproval,
) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "principal": row.principal,
        "tool_name": row.tool_name,
        "arguments": serde_json::from_str::<serde_json::Value>(&row.arguments)
            .unwrap_or(serde_json::Value::Null),
        "risk_tier": row.risk_tier,
        "requested_at": row.requested_at,
        "status": row.status,
        "resolved_at": row.resolved_at,
        "resolved_by": row.resolved_by,
    })
}

/// GET /api/approvals?status=pending
pub async fn handle_list_approvals(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, JsonErr> {
    let _principal = authorize(&state, &peer, &headers)?;
    let data_dir = state.config.read().data_dir.clone();
    let store = zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore::shared(
        &data_dir,
    )
    .map_err(|e| disabled(&format!("pending-approval store unavailable: {e}")))?;
    let rows = store
        .list(query.status.as_deref())
        .map_err(|e| disabled(&format!("list failed: {e}")))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "approvals": rows.iter().map(pending_approval_json).collect::<Vec<_>>()
        })),
    ))
}

#[derive(Deserialize)]
pub struct ResolveBody {
    /// `"approve"` | `"deny"`.
    decision: String,
}

/// POST /api/approvals/{id}/resolve
pub async fn handle_resolve_approval(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<impl IntoResponse, JsonErr> {
    let resolved_by = authorize(&state, &peer, &headers)?;
    let data_dir = state.config.read().data_dir.clone();
    let store = zeroclaw_runtime::control_plane::pending_approvals::PendingApprovalsStore::shared(
        &data_dir,
    )
    .map_err(|e| disabled(&format!("pending-approval store unavailable: {e}")))?;

    let row = store
        .get(&id)
        .map_err(|e| disabled(&format!("lookup failed: {e}")))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no pending approval with id {id}") })),
            )
        })?;
    if row.status != "pending" {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "outcome": "already_resolved",
                "id": id,
                "status": row.status,
            })),
        ));
    }

    match body.decision.as_str() {
        "deny" => {
            store
                .resolve(&id, "denied", resolved_by)
                .map_err(|e| disabled(&format!("resolve failed: {e}")))?;
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "outcome": "denied", "id": id })),
            ))
        }
        "approve" => {
            store
                .resolve(&id, "approved", resolved_by)
                .map_err(|e| disabled(&format!("resolve failed: {e}")))?;
            // The decision is durably recorded above regardless of what
            // happens next — a downstream execution failure (Stripe down,
            // MCP server unreachable) must not un-record a human's real
            // "yes, do this" decision. Execution is a best-effort follow-up
            // reported alongside the resolve outcome, not a precondition of it.
            let execution = execute_approved_tool(&state, &row).await;
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "outcome": "approved",
                    "id": id,
                    "execution": execution,
                })),
            ))
        }
        other => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("decision must be \"approve\" or \"deny\", got \"{other}\"")
            })),
        )),
    }
}

/// Execute one already-approved MCP-origin tool call directly, out-of-band.
/// Scoped narrowly: this reconnects only the ONE named server (parsed from
/// `<server>__<tool>`), entity-scoped to the row's own `principal` — it does
/// NOT re-derive or re-check the originating agent/tenant-type grant
/// (`agent_type_mcp_bundles`), because that check already gated whether this
/// row could be *created* in the first place (`gate_tool_approval`); this is
/// authorization to run the ALREADY-approved call, not a fresh grant.
///
/// `pub(crate)`: also called by `api_tenant_approvals::resolve_and_continue_
/// tenant_approval` (patch 0029) — the tool-execution mechanics are identical
/// for the loopback-admin and tenant-scoped-resume paths; only what happens
/// after execution differs (nothing further here vs. a continuation turn
/// there).
/// ADR-006 Part B fallback for [`execute_approved_tool`]: re-resolves a
/// tenant custom MCP server directly from `row.principal`/`row.agent_type`
/// (the same authenticated source of truth a live turn's tenant scoping
/// already trusts, never agent/LLM output) when `server_name` isn't a
/// curated `[[mcp.servers]]` entry. Returns `None` for anything that isn't
/// even shaped like a tenant custom server name (missing the
/// `tenant_`-prefix), a row with no `agent_type`/`principal` (can't
/// meaningfully resolve — pre-dates patch 0028's context columns, or a
/// loopback/CLI-created row), or a resolver miss (server was deleted,
/// disabled, or never verified) — all of which fall through to
/// `execute_approved_tool`'s own "no such server" error.
async fn resolve_tenant_custom_server_config(
    row: &zeroclaw_runtime::control_plane::pending_approvals::PendingApproval,
    server_name: &str,
) -> Option<zeroclaw_config::schema::McpServerConfig> {
    let tenant_name =
        server_name.strip_prefix(zeroclaw_runtime::agent::tenant::TENANT_CUSTOM_MCP_NAME_PREFIX)?;
    let agent_type = row.agent_type.as_deref()?;
    if row.principal.is_empty() {
        return None;
    }
    let sel = crate::tenant::TenantSelector {
        user_id: row.principal.clone(),
        agent_type: agent_type.to_string(),
    };
    let servers = crate::tenant::TenantCustomMcpResolver::global()
        .resolve(&sel)
        .await;
    let matched = servers.into_iter().find(|s| s.name == tenant_name)?;
    zeroclaw_runtime::agent::tenant::tenant_custom_mcp_server_configs(std::slice::from_ref(
        &matched,
    ))
    .into_iter()
    .next()
}

pub(crate) async fn execute_approved_tool(
    state: &AppState,
    row: &zeroclaw_runtime::control_plane::pending_approvals::PendingApproval,
) -> serde_json::Value {
    let Some((server_name, _)) = row.tool_name.split_once("__") else {
        return serde_json::json!({
            "success": false,
            "error": format!(
                "'{}' is not an MCP-origin tool (no <server>__<tool> separator) — \
                 out-of-band execution only supports MCP tools today",
                row.tool_name
            ),
        });
    };

    let config = state.config.read().clone();
    let server_config = match config
        .mcp
        .servers
        .iter()
        .find(|s| s.name == server_name)
        .cloned()
    {
        Some(s) => Some(s),
        // ADR-006 Part B: a tenant custom MCP server is never written into
        // `config.mcp.servers` — it's synthesized fresh per-turn from
        // `TenantCustomMcpResolver`. This out-of-band executor has no live
        // `TenantContext` to read from (it runs off a stored row, not a
        // task-local turn), so on a static-config miss it re-resolves
        // directly here instead.
        None => resolve_tenant_custom_server_config(row, server_name).await,
    };
    let Some(server_config) = server_config else {
        return serde_json::json!({
            "success": false,
            "error": format!("no [[mcp.servers]] entry named '{server_name}' in current config"),
        });
    };
    let principal = (!row.principal.is_empty()).then_some(row.principal.as_str());
    let Some(scoped_server) = apply_tenant_entity_scoping(server_config, principal) else {
        return serde_json::json!({
            "success": false,
            "error": "server is tenant-gated but this row has no principal, or its url \
                      failed to parse — refusing to connect unscoped",
        });
    };

    let arguments: serde_json::Value = match serde_json::from_str(&row.arguments) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("stored arguments are not valid JSON: {e}"),
            });
        }
    };

    // F-2: claim before executing, same as the live turn path would have —
    // an operator double-clicking "approve" (or a retried POST) must not
    // double-fire the real side effect.
    let idem_key = zeroclaw_runtime::control_plane::tool_idem::derive_key(
        principal.unwrap_or(""),
        &row.id,
        &row.id,
        &row.tool_name,
        &row.arguments,
    );
    let ledger = match zeroclaw_runtime::control_plane::tool_idem::ToolIdemLedger::shared(
        &config.data_dir,
    ) {
        Ok(l) => l,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("could not open idempotency ledger: {e}"),
            });
        }
    };
    match ledger.claim(&idem_key) {
        Ok(zeroclaw_runtime::control_plane::tool_idem::Claim::AlreadyDone(output)) => {
            return serde_json::json!({ "success": true, "output": output, "note": "already executed previously" });
        }
        Ok(zeroclaw_runtime::control_plane::tool_idem::Claim::InFlight) => {
            return serde_json::json!({
                "success": false,
                "error": "already in progress from an earlier approve — not re-executing",
            });
        }
        Ok(zeroclaw_runtime::control_plane::tool_idem::Claim::Claimed) => {}
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("idempotency claim failed: {e}"),
            });
        }
    }

    let registry =
        match zeroclaw_tools::mcp_client::McpRegistry::connect_all(&[scoped_server]).await {
            Ok(r) => r,
            Err(e) => {
                let _ = ledger.release(&idem_key);
                return serde_json::json!({
                    "success": false,
                    "error": format!("could not connect to MCP server '{server_name}': {e}"),
                });
            }
        };
    match registry.call_tool(&row.tool_name, arguments).await {
        Ok(output) => {
            let _ = ledger.complete(&idem_key, &output);
            serde_json::json!({ "success": true, "output": output })
        }
        Err(e) => {
            let _ = ledger.release(&idem_key);
            serde_json::json!({ "success": false, "error": e.to_string() })
        }
    }
}

#[cfg(test)]
mod tenant_custom_fallback_tests {
    use super::*;
    use zeroclaw_runtime::control_plane::pending_approvals::PendingApproval;

    fn base_row() -> PendingApproval {
        PendingApproval {
            id: "pa_test".to_string(),
            principal: "u_test".to_string(),
            tool_name: "tenant_orders__get_current_time".to_string(),
            arguments: "{}".to_string(),
            risk_tier: "irreversible".to_string(),
            requested_at: "2026-08-15T00:00:00Z".to_string(),
            status: "approved".to_string(),
            resolved_at: None,
            resolved_by: None,
            tenant_id: Some("u_test.customer_service".to_string()),
            agent_type: Some("customer_service".to_string()),
            session_id: None,
            origin_message: None,
            delivered_at: None,
        }
    }

    #[tokio::test]
    async fn returns_none_for_non_tenant_prefixed_name() {
        // A curated server name never carries the tenant_ prefix — this
        // path must never be reached for one (the caller only falls back
        // here on a static-config miss, but the guard is here too as
        // defense-in-depth against a future caller change).
        let row = base_row();
        assert!(
            resolve_tenant_custom_server_config(&row, "composio-zendesk-support")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn returns_none_when_row_has_no_agent_type() {
        // Predates patch 0028's context columns, or a loopback/CLI-created
        // row — nothing to resolve a tenant's custom servers against.
        let mut row = base_row();
        row.agent_type = None;
        assert!(
            resolve_tenant_custom_server_config(&row, "tenant_orders")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn returns_none_when_row_has_empty_principal() {
        let mut row = base_row();
        row.principal = String::new();
        assert!(
            resolve_tenant_custom_server_config(&row, "tenant_orders")
                .await
                .is_none()
        );
    }
}
