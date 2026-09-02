//! Cerveau (White-Box Memory, patch 0040): tenant-scoped browse/edit/delete
//! for a single tenant's own memory rows — the Postgres-backed key/value
//! store (`zeroclaw-memory`, categories core/daily/conversation/document),
//! NOT the separate cognee-rs graph memory, which this module has nothing
//! to do with.
//!
//! ## Why new routes instead of fixing `/api/memory` in place
//!
//! `/api/memory` already exists (`crate::api::handle_api_memory_*`) and, as
//! of this patch, `handle_api_memory_list`/`handle_api_memory_delete` are
//! ALSO fixed to thread a tenant selector through
//! [`crate::api::resolve_memory_handle_scoped`] — they used to silently drop
//! it and always resolve the install-wide/per-agent-alias handle (see that
//! module's doc comments for the full history of that bug). But fixing the
//! resolver call doesn't change `/api/memory`'s *authentication*: every
//! handler there still gates on [`crate::api::require_auth`], which checks
//! the operator's `PairingGuard` bearer token — proof that a human paired
//! their own dashboard/CLI session with this daemon, not proof of which
//! tenant a request is acting on behalf of, and not a credential an
//! external, multi-tenant service like avry-backend would ever hold (it
//! isn't "the operator"; it's a backend acting on behalf of many different
//! end-users, none of whom have paired anything).
//!
//! What avry-backend *does* hold is the same shared secret the bridge
//! already sends on every other tenant-scoped call: `X-Webhook-Secret`. So
//! these routes follow `api_tenant_approvals.rs`'s established two-layer
//! contract exactly: (1) `X-Webhook-Secret`, constant-time-compared against
//! `state.webhook_secret_hash`, proves "this is a legitimate service
//! caller"; (2) `X-Tenant-Id` + `X-Agent-Type` ([`TenantSelector`]) name
//! *whose* memory the call may touch. Neither layer alone is sufficient — a
//! valid secret proves the caller is the bridge/backend, never which
//! tenant's data it's asking for; see `api_tenant_approvals.rs`'s own doc
//! for the same reasoning applied to pending approvals.
//!
//! ## Isolation is not reimplemented here
//!
//! Every handler below resolves its `Memory` handle through the exact same
//! [`crate::api::resolve_memory_handle_scoped`] → `create_memory_for_tenant`
//! → `AgentScopedMemory` path a live tenant turn already rides (see
//! `create_memory_for_tenant`'s own doc in `zeroclaw-memory/src/lib.rs`).
//! Concretely, for the two mutating operations this module exposes:
//!
//! - **DELETE** goes through `Memory::forget`, which — for an
//!   `AgentScopedMemory` handle — first tries
//!   `inner.forget_for_agent(key, self.agent_id)`. Every backing store's
//!   `forget_for_agent` is a `DELETE ... WHERE key = ? AND agent_id = ?`
//!   (sqlite: `crates/zeroclaw-memory/src/sqlite.rs:1679-1693`; postgres:
//!   `crates/zeroclaw-memory/src/postgres.rs:738-751`) — the tenant
//!   isolation is a SQL `WHERE` clause, not an application-level filter, so
//!   this delete cannot reach another tenant's row of the same key even in
//!   principle.
//! - **EDIT** (existence check + `Memory::store`) uses `Memory::get`, which
//!   for `AgentScopedMemory` calls `inner.get_for_agent(key,
//!   self.agent_id)` first — same structural `WHERE key = ? AND agent_id =
//!   ?` shape (sqlite: `sqlite.rs:1548-1563`; postgres:
//!   `postgres.rs:665-688`). `Memory::store` itself is a real per-backend
//!   `UPSERT` keyed on `(agent_id, key)` (sqlite:
//!   `ON CONFLICT(agent_id, key) DO UPDATE SET ...` at `sqlite.rs:438`;
//!   postgres: `ON CONFLICT (agent_id, key) DO UPDATE SET ...` at
//!   `postgres.rs:940`), so calling it again with the same key edits the
//!   existing row rather than creating a duplicate — there is no separate
//!   "edit" primitive in the `Memory` trait, nor does this module need one.
//!
//! **LIST is the one exception worth flagging explicitly.**
//! `AgentScopedMemory::list` (`crates/zeroclaw-memory/src/agent_scoped.rs:
//! 267-281`) calls the *unscoped* `inner.list(category, session_id)` — which
//! runs a plain `SELECT ... FROM memories` with no `agent_id` predicate at
//! all (postgres has no row cap on this query; sqlite caps it at 1000 rows
//! **across every agent/tenant in the install**) — and only *then* filters
//! the returned rows down to `self.allowed_agent_ids` in Rust, the same
//! "SQL result set, then an app-level ownership filter" shape
//! `api_tenant_approvals.rs`'s own list handler uses for pending-approval
//! rows. This is pre-existing behavior in `AgentScopedMemory` (unchanged by
//! this patch, and already relied on by every other list-mode caller,
//! including the fixed `/api/memory` handler above) — it does not leak
//! another tenant's rows to this module's caller, since the filter is
//! applied before anything is returned. But on a busy multi-tenant install
//! it is a real correctness/scale risk in the other direction: if other
//! tenants' rows fill the (sqlite) 1000-row cap, or postgres's full-table
//! scan simply grows slow, a tenant's own older rows can silently be pushed
//! out of — or just slow to reach — its own list view, purely because of
//! how much *other* tenants have stored. Worth a follow-up in
//! `AgentScopedMemory::list` itself (an `agent_id = ANY($1)` / `IN (...)`
//! predicate pushed into the SQL, mirroring how `recall_for_agents` already
//! does it) — out of scope for this patch, which only adds the HTTP
//! surface on top of the existing (correct-but-unscaled) primitive.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use zeroclaw_runtime::security::pairing::constant_time_eq;

use crate::AppState;
use crate::api::{
    MemoryDeleteQuery, MemoryQuery, resolve_memory_handle_scoped, sanitize_memory_entries_for_api,
};
use crate::tenant::TenantSelector;

type JsonErr = (StatusCode, Json<serde_json::Value>);

fn unauthorized(msg: &str) -> JsonErr {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn not_found(msg: impl Into<String>) -> JsonErr {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

/// Layer 1 alone, pulled out as a pure function of `configured_hash` (rather
/// than taking `&AppState`) so it's testable without constructing this
/// crate's large `AppState` literal (`api_tenant_approvals.rs` sidesteps the
/// same problem by only unit-testing functions that never touch `AppState`
/// at all; this does the equivalent by narrowing the parameter to just the
/// one field these checks actually read). Reject with 401 if no webhook
/// secret is configured on this deployment; reject with 401 on a
/// missing/invalid `X-Webhook-Secret`; otherwise succeed.
fn verify_webhook_secret(
    configured_hash: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), JsonErr> {
    let Some(secret_hash) = configured_hash else {
        return Err(unauthorized(
            "tenant-scoped memory access requires X-Webhook-Secret auth on this deployment",
        ));
    };
    let header_hash = headers
        .get("X-Webhook-Secret")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::hash_webhook_secret);
    match header_hash {
        Some(val) if constant_time_eq(&val, secret_hash) => Ok(()),
        _ => Err(unauthorized("invalid or missing X-Webhook-Secret header")),
    }
}

/// Layer 2 alone: `X-Tenant-Id`/`X-Agent-Type` must be present and
/// well-formed. A half-specified pair (one header present, one missing) is
/// rejected the same as both missing — see [`TenantSelector::from_headers`]'s
/// own doc for why a typo must never fall through to "no tenant".
fn verify_tenant_headers(headers: &HeaderMap) -> Result<TenantSelector, JsonErr> {
    match TenantSelector::from_headers(headers) {
        Ok(Some(sel)) => Ok(sel),
        Ok(None) => Err(unauthorized(
            "X-Tenant-Id and X-Agent-Type are required to access tenant-scoped memory",
        )),
        Err(reason) => Err(unauthorized(reason)),
    }
}

/// Both auth layers together, factored into one function since this module
/// has three handlers (`api_tenant_approvals.rs` duplicates the same
/// two-layer check inline across its two handlers; with a third handler
/// here, duplicating a third time risks one of them silently drifting from
/// the other two, so this is pulled out instead).
fn authorize_tenant_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TenantSelector, JsonErr> {
    verify_webhook_secret(state.webhook_secret_hash.as_deref(), headers)?;
    verify_tenant_headers(headers)
}

fn parse_category(cat: &str) -> zeroclaw_memory::MemoryCategory {
    match cat {
        "core" => zeroclaw_memory::MemoryCategory::Core,
        "daily" => zeroclaw_memory::MemoryCategory::Daily,
        "conversation" => zeroclaw_memory::MemoryCategory::Conversation,
        other => zeroclaw_memory::MemoryCategory::Custom(other.to_string()),
    }
}

/// GET /webhook/memory — list or search the caller's own tenant memory.
///
/// Query params mirror `/api/memory`'s [`MemoryQuery`] exactly (`query`,
/// `category`, `since`, `until`, `agent`) — same shape, different auth. The
/// `agent` param names the *host* `[agents.<alias>]` entry whose memory
/// backend the tenant overlay borrows; it is a separate axis from
/// `X-Agent-Type` (see `TenantContext::agent_type`'s own doc for why the two
/// must never be conflated: `X-Agent-Type` selects a persona/skill-bundle,
/// never a config-alias provisioning decision) and is required — omitting
/// it 400s from [`resolve_memory_handle_scoped`] rather than silently
/// picking a host, by that function's own design.
pub async fn handle_webhook_memory_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<MemoryQuery>,
) -> impl IntoResponse {
    let sel = match authorize_tenant_request(&state, &headers) {
        Ok(sel) => sel,
        Err(e) => return e.into_response(),
    };

    let mem = match resolve_memory_handle_scoped(&state, params.agent.as_deref(), Some(&sel)).await
    {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    // Same recall-vs-list branching as `handle_api_memory_list`: a query or
    // a time bound switches to keyword recall, otherwise it's a plain
    // category-filtered listing.
    if params.query.is_some() || params.since.is_some() || params.until.is_some() {
        let query = params.query.as_deref().unwrap_or("");
        let since = params.since.as_deref();
        let until = params.until.as_deref();
        match mem.recall(query, 50, None, since, until).await {
            Ok(entries) => {
                let entries = match params.category.as_deref() {
                    Some(cat) => entries
                        .into_iter()
                        .filter(|e| e.category.to_string() == cat)
                        .collect(),
                    None => entries,
                };
                Json(serde_json::json!({
                    "entries": sanitize_memory_entries_for_api(entries)
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Memory recall failed: {e}")})),
            )
                .into_response(),
        }
    } else {
        let category = params.category.as_deref().map(parse_category);
        match mem.list(category.as_ref(), None).await {
            Ok(entries) => Json(serde_json::json!({
                "entries": sanitize_memory_entries_for_api(entries)
            }))
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Memory list failed: {e}")})),
            )
                .into_response(),
        }
    }
}

#[derive(Deserialize)]
pub struct TenantMemoryEditBody {
    pub content: String,
    /// Omit to keep the entry's existing category.
    pub category: Option<String>,
}

/// PUT /webhook/memory/{key} — edit an existing entry's content (and,
/// optionally, its category), scoped to the caller's own tenant.
///
/// This is deliberately an *edit*, not an upsert-that-can-create: it 404s
/// if `key` doesn't already exist for this tenant, via a `Memory::get`
/// existence check before calling `Memory::store` (see this module's own
/// doc for why that `get` is itself structurally tenant-scoped, so this
/// never confirms whether a *different* tenant happens to hold that key).
/// White-Box Memory is a view/edit/delete surface over what an agent
/// already remembered about a user, not a way to plant new memories from
/// the dashboard — if that need shows up later it should be a deliberate,
/// separate `POST`, not a side effect of this route accepting an unknown
/// key.
pub async fn handle_webhook_memory_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(query): Query<MemoryDeleteQuery>,
    Json(body): Json<TenantMemoryEditBody>,
) -> impl IntoResponse {
    let sel = match authorize_tenant_request(&state, &headers) {
        Ok(sel) => sel,
        Err(e) => return e.into_response(),
    };

    let mem = match resolve_memory_handle_scoped(&state, query.agent.as_deref(), Some(&sel)).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    let existing = match mem.get(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return not_found(format!("no memory entry with key {key:?} for this tenant"))
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Memory lookup failed: {e}")})),
            )
                .into_response();
        }
    };

    let category = body
        .category
        .as_deref()
        .map(parse_category)
        .unwrap_or(existing.category);

    if let Err(e) = mem.store(&key, &body.content, category, None).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Memory store failed: {e}")})),
        )
            .into_response();
    }

    match mem.get(&key).await {
        Ok(Some(entry)) => Json(serde_json::json!({
            "status": "ok",
            "entry": sanitize_memory_entries_for_api(vec![entry]).pop()
        }))
        .into_response(),
        // The write above succeeded; a failure to read it straight back is
        // surfaced as a degraded-but-successful response rather than an
        // error, since the edit itself is already durable.
        Ok(None) => Json(serde_json::json!({"status": "ok", "entry": null})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("edit succeeded but re-reading the entry failed: {e}")
            })),
        )
            .into_response(),
    }
}

/// DELETE /webhook/memory/{key} — remove an entry, scoped to the caller's
/// own tenant. See this module's own doc for why the underlying
/// `Memory::forget` call is structurally incapable of reaching another
/// tenant's row of the same key.
pub async fn handle_webhook_memory_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(query): Query<MemoryDeleteQuery>,
) -> impl IntoResponse {
    let sel = match authorize_tenant_request(&state, &headers) {
        Ok(sel) => sel,
        Err(e) => return e.into_response(),
    };

    let mem = match resolve_memory_handle_scoped(&state, query.agent.as_deref(), Some(&sel)).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    match mem.forget(&key).await {
        Ok(deleted) => {
            Json(serde_json::json!({"status": "ok", "deleted": deleted})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Memory forget failed: {e}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn verify_webhook_secret_rejects_when_none_is_configured() {
        let headers = headers_with(&[("X-Webhook-Secret", "whatever")]);
        let err = verify_webhook_secret(None, &headers).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_webhook_secret_rejects_a_missing_header() {
        let configured = crate::hash_webhook_secret("s3cr3t");
        let headers = HeaderMap::new();
        let err = verify_webhook_secret(Some(&configured), &headers).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_webhook_secret_rejects_a_wrong_secret() {
        let configured = crate::hash_webhook_secret("s3cr3t");
        let headers = headers_with(&[("X-Webhook-Secret", "not-the-secret")]);
        let err = verify_webhook_secret(Some(&configured), &headers).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_webhook_secret_accepts_the_right_secret() {
        let configured = crate::hash_webhook_secret("s3cr3t");
        let headers = headers_with(&[("X-Webhook-Secret", "s3cr3t")]);
        assert!(verify_webhook_secret(Some(&configured), &headers).is_ok());
    }

    #[test]
    fn verify_tenant_headers_rejects_when_both_are_missing() {
        let headers = HeaderMap::new();
        let err = verify_tenant_headers(&headers).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_tenant_headers_rejects_a_half_specified_pair() {
        // X-Agent-Type deliberately omitted: TenantSelector::from_headers
        // must reject this as malformed rather than treating it as "no
        // tenant" (which would let a typo'd header silently fall through to
        // unscoped-request handling elsewhere).
        let headers = headers_with(&[("X-Tenant-Id", "u1")]);
        let err = verify_tenant_headers(&headers).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_tenant_headers_accepts_a_well_formed_pair() {
        let headers = headers_with(&[("X-Tenant-Id", "u1"), ("X-Agent-Type", "customer_service")]);
        let sel = verify_tenant_headers(&headers).unwrap();
        assert_eq!(sel.user_id, "u1");
        assert_eq!(sel.agent_type, "customer_service");
        assert_eq!(sel.tenant_id(), "u1.customer_service");
    }

    #[test]
    fn parse_category_maps_known_names_and_falls_back_to_custom() {
        assert_eq!(
            parse_category("core"),
            zeroclaw_memory::MemoryCategory::Core
        );
        assert_eq!(
            parse_category("daily"),
            zeroclaw_memory::MemoryCategory::Daily
        );
        assert_eq!(
            parse_category("conversation"),
            zeroclaw_memory::MemoryCategory::Conversation
        );
        assert_eq!(
            parse_category("document"),
            zeroclaw_memory::MemoryCategory::Custom("document".to_string())
        );
    }
}
