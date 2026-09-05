//! Cerveau (ADR-008 Phase 4): a tenant-scoped, read-only listing of the
//! skills a tenant's agent turn actually has available — the "read-only
//! Skills listing per agent" Phase 4's exit gate calls for. Resolves
//! through the exact same [`SkillsService::resolve_effective_skills`] the
//! loopback `/api/agents/{alias}/skills` route uses, so there is one place
//! that decides what an agent's effective skill set is.
//!
//! Same two-layer `X-Webhook-Secret` + [`TenantSelector`] contract as
//! `api_tenant_approvals.rs`/`api_tenant_memory.rs`: (1) the secret proves
//! the caller is a legitimate service (the bridge/avry-backend), never
//! which tenant it's asking on behalf of; (2) `X-Tenant-Id`/`X-Agent-Type`
//! name that. Duplicated here rather than shared, following those two
//! modules' own precedent — `api_tenant_memory.rs`'s doc comment already
//! flagged the duplication risk at two copies and a third was written
//! anyway. This makes four. A real cleanup candidate (extract to a shared
//! `tenant_auth` module), not a decision to keep re-making — noted rather
//! than silently repeated a fifth time.
//!
//! **No `agent` selector, unlike `/webhook/memory`.** A tenant's
//! `agent_type` header (`customer_service`, `leads_qualifier`, …) is a
//! product-facing persona label, not a Cerveau config alias — this
//! install's `config.toml` has no `[agents.customer_service]` section at
//! all, only the delegation-mesh brains (`analyst_brain`, `security_brain`,
//! …). Every tenant turn already runs on the one alias
//! `resolve_gateway_chat_agent_alias`'s own fallback picks with no override
//! — `config.resolved_runtime_agent_alias()`, the same call the plain
//! `/webhook` turn and `cron::tenant_sync`'s reconcile both use. Asking the
//! tenant to name a Cerveau-internal alias they have no way to know would
//! only 400; resolving the same alias every other tenant-facing surface
//! already resolves is the correct behaviour, not a shortcut.
//!
//! **Trimmed on purpose.** The loopback route's full `AgentSkillEntry` also
//! carries `directory` (a server filesystem path) and `editable` — neither
//! means anything on a route with no corresponding write surface, and a
//! path is exactly the kind of implementation detail an external-facing API
//! should not leak. Dropped/shadowed-skill detail is left out too: it's
//! operator debugging information about *why* a skill failed to load, not
//! something a tenant did or can act on.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use zeroclaw_runtime::security::pairing::constant_time_eq;
use zeroclaw_runtime::skills::{EffectiveSkill, SkillOrigin, SkillsService};

use crate::AppState;
use crate::api_skills::service_error_response;
use crate::tenant::TenantSelector;

type JsonErr = (StatusCode, Json<serde_json::Value>);

fn unauthorized(msg: &str) -> JsonErr {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": msg })),
    )
}

/// Layer 1: reject with 401 if no webhook secret is configured on this
/// deployment, or on a missing/invalid `X-Webhook-Secret`; otherwise
/// succeed. Mirrors `api_tenant_memory.rs::verify_webhook_secret` exactly.
fn verify_webhook_secret(
    configured_hash: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), JsonErr> {
    let Some(secret_hash) = configured_hash else {
        return Err(unauthorized(
            "tenant-scoped skills listing requires X-Webhook-Secret auth on this deployment",
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

/// Layer 2: `X-Tenant-Id`/`X-Agent-Type` must be present and well-formed.
/// Required even though the resolved skill set is currently the same for
/// every tenant (§ above) — this stays an authenticated, tenant-attributed
/// call rather than becoming an unauthenticated info surface, and a future
/// install with per-`agent_type` aliases would need the selector threaded
/// through exactly here.
fn verify_tenant_headers(headers: &HeaderMap) -> Result<TenantSelector, JsonErr> {
    match TenantSelector::from_headers(headers) {
        Ok(Some(sel)) => Ok(sel),
        Ok(None) => Err(unauthorized(
            "X-Tenant-Id and X-Agent-Type are required to list this agent's skills",
        )),
        Err(reason) => Err(unauthorized(reason)),
    }
}

fn authorize_tenant_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TenantSelector, JsonErr> {
    verify_webhook_secret(state.webhook_secret_hash.as_deref(), headers)?;
    verify_tenant_headers(headers)
}

/// Flat wire shape for one skill — `AgentSkillEntry` with `directory`,
/// `editable` and `shadowed` dropped (see module doc).
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct TenantSkillEntry {
    pub name: String,
    pub description: String,
    /// `"workspace"` | `"open-skills"` | `"plugin"` | `"bundle"`.
    pub origin: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct TenantSkillsResult {
    pub skills: Vec<TenantSkillEntry>,
}

fn tenant_skill_entry(s: EffectiveSkill) -> TenantSkillEntry {
    let origin = match s.origin {
        SkillOrigin::Workspace => "workspace",
        SkillOrigin::OpenSkills => "open-skills",
        SkillOrigin::Plugin(_) => "plugin",
        SkillOrigin::Bundle(_) => "bundle",
    };
    TenantSkillEntry {
        name: s.name,
        description: s.description,
        origin: origin.to_string(),
    }
}

/// `GET /webhook/skills`
pub async fn handle_webhook_skills(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize_tenant_request(&state, &headers) {
        return e.into_response();
    }
    let config = state.config.read().clone();
    let Some(alias) = config.resolved_runtime_agent_alias().map(str::to_owned) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "no configured [agents.<alias>] entry to list skills for"
            })),
        )
            .into_response();
    };
    let install_root = config.install_root_dir();
    let service = SkillsService::new(&config, install_root);

    match service.resolve_effective_skills(&alias) {
        Ok(set) => Json(TenantSkillsResult {
            skills: set.skills.into_iter().map(tenant_skill_entry).collect(),
        })
        .into_response(),
        Err(e) => service_error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn verify_webhook_secret_rejects_when_unconfigured() {
        let headers = headers_with(&[("X-Webhook-Secret", "anything")]);
        assert!(verify_webhook_secret(None, &headers).is_err());
    }

    #[test]
    fn verify_webhook_secret_rejects_missing_header() {
        let hash = crate::hash_webhook_secret("real-secret");
        let headers = HeaderMap::new();
        assert!(verify_webhook_secret(Some(&hash), &headers).is_err());
    }

    #[test]
    fn verify_webhook_secret_rejects_wrong_value() {
        let hash = crate::hash_webhook_secret("real-secret");
        let headers = headers_with(&[("X-Webhook-Secret", "wrong-secret")]);
        assert!(verify_webhook_secret(Some(&hash), &headers).is_err());
    }

    #[test]
    fn verify_webhook_secret_accepts_the_right_value() {
        let hash = crate::hash_webhook_secret("real-secret");
        let headers = headers_with(&[("X-Webhook-Secret", "real-secret")]);
        assert!(verify_webhook_secret(Some(&hash), &headers).is_ok());
    }

    #[test]
    fn verify_tenant_headers_rejects_a_half_specified_pair() {
        // The same "typo must not fall through to no-tenant" contract
        // TenantSelector::from_headers itself documents.
        let headers = headers_with(&[("X-Tenant-Id", "u1")]);
        assert!(verify_tenant_headers(&headers).is_err());
    }

    #[test]
    fn verify_tenant_headers_accepts_a_complete_pair() {
        let headers = headers_with(&[("X-Tenant-Id", "u1"), ("X-Agent-Type", "customer_service")]);
        let sel = verify_tenant_headers(&headers).expect("a complete pair must resolve");
        assert_eq!(sel.user_id, "u1");
        assert_eq!(sel.agent_type, "customer_service");
    }

    #[test]
    fn tenant_skill_entry_drops_directory_editable_and_shadowed() {
        // The whole point of this module's own trimmed shape: verified by
        // construction rather than by inspecting serialized JSON for the
        // absence of fields that were never on the struct to begin with.
        let effective = EffectiveSkill {
            name: "pdf-export".to_string(),
            description: "Export a report to PDF".to_string(),
            origin: SkillOrigin::Bundle("core".to_string()),
            directory: Some(std::path::PathBuf::from("/srv/cerveau/skills/pdf-export")),
            editable: true,
            bundle: Some("core".to_string()),
            shadowed: vec![],
        };
        let entry = tenant_skill_entry(effective);
        assert_eq!(entry.name, "pdf-export");
        assert_eq!(entry.description, "Export a report to PDF");
        assert_eq!(entry.origin, "bundle");
        // TenantSkillEntry has no `directory`/`editable`/`shadowed` fields
        // at all — this compiling with exactly those three fields set above
        // is the proof.
    }

    #[test]
    fn tenant_skill_entry_maps_every_origin_variant() {
        let make = |origin: SkillOrigin| EffectiveSkill {
            name: "s".to_string(),
            description: "d".to_string(),
            origin,
            directory: None,
            editable: false,
            bundle: None,
            shadowed: vec![],
        };
        assert_eq!(
            tenant_skill_entry(make(SkillOrigin::Workspace)).origin,
            "workspace"
        );
        assert_eq!(
            tenant_skill_entry(make(SkillOrigin::OpenSkills)).origin,
            "open-skills"
        );
        assert_eq!(
            tenant_skill_entry(make(SkillOrigin::Plugin("p".into()))).origin,
            "plugin"
        );
        assert_eq!(
            tenant_skill_entry(make(SkillOrigin::Bundle("b".into()))).origin,
            "bundle"
        );
    }
}
