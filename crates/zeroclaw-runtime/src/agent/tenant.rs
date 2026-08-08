//! Cerveau: per-request tenant identity context.
//!
//! A *tenant* is a dynamically-provisioned identity (a row in the platform
//! database — Aivory `user_id × agent_type`), not an `[agents.<alias>]`
//! config entry. At 10k+ tenants the config-alias machinery (TOML entry +
//! workspace dir + reload per identity) cannot serve this; instead a turn
//! runs on a *host* agent alias with a tenant overlay:
//!
//! - **Memory** is created via `zeroclaw_memory::create_memory_for_tenant`,
//!   binding the agent-id dimension to the tenant with an empty cross-agent
//!   allowlist (structurally jailed — see that factory's docs).
//! - **Persona** (operator-configured identity: name, business context,
//!   tone, languages) is appended to the system prompt as pre-framed inert
//!   data. It is untrusted input and must never carry instructions that
//!   override security rules; the gateway renders it through the ingress
//!   framing conventions before it reaches this crate.
//! - **Attribution**: task records created during a tenant turn stamp the
//!   tenant into `principal_id` (upstream's EPIC-D seam).
//!
//! The context is threaded as a tokio task-local — the same pattern as
//! [`crate::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT`] — so the giant
//! `process_message` signature and its many call sites stay untouched
//! (rebase-friendliness: this module plus small hooks, not a plumbing
//! rewrite). Callers (the gateway webhook handler) scope it around the
//! turn future; everything inside the turn reads it via
//! [`current_tenant`]. Absent context = vanilla single-operator behavior,
//! bit-for-bit.

use std::sync::Arc;

/// Immutable per-turn tenant overlay. Built by the gateway after
/// authenticating the request and resolving the tenant's persona from the
/// platform database.
#[derive(Debug, Clone)]
pub struct TenantContext {
    /// Validated tenant identifier, `<user_id>:<agent_type>` shaped into a
    /// single `[A-Za-z0-9._-]+` token by the gateway. Used (with a `t_`
    /// namespace prefix) as the memory agent-id dimension and as the
    /// `principal_id` stamped on task records.
    pub tenant_id: String,
    /// The raw platform user id (`X-Tenant-Id`, pre-flattening), distinct
    /// from `tenant_id` which also folds in the agent type. Per ADR-002 D2,
    /// third-party OAuth connections (Composio) are per-*user*, shared
    /// across every agent the user deploys — so entity-scoped tool access
    /// (Phase 4.1, `McpServerConfig::tenant_entity_query_param`) must key
    /// off this field, not `tenant_id`. Authenticated by the gateway same
    /// as `tenant_id`; never derived from agent output or message content.
    pub platform_user_id: String,
    /// The tenant's Aivory agent type (`autonomous`, `customer_service`,
    /// `leads_qualifier`, `finance_invoice_ops`, `office_assistant`) — the
    /// raw `X-Agent-Type` header value, authenticated by the gateway same
    /// as `tenant_id`/`platform_user_id`.
    ///
    /// This is deliberately *not* used to select which host
    /// `[agents.<alias>]` a turn runs on (that's `?agent=`, resolved
    /// independently — see `zeroclaw-gateway`'s
    /// `resolve_gateway_chat_agent_alias`, and the current bridge's own
    /// `telegram-agent.js` for the reference pattern: one running process,
    /// `agent_type` is a per-request data value that dynamically selects
    /// prompt/tools, never a provisioning axis). Here it drives which
    /// *additional* skill bundles this turn loads on top of whatever the
    /// host alias already grants — see
    /// `Config::skill_bundle_aliases_for_tenant` (Phase 4.1 follow-on,
    /// patch 0011) — the same data-driven pattern already used for
    /// persona (this struct) and Composio entity scoping
    /// (`platform_user_id`, patch 0010).
    pub agent_type: String,
    /// Pre-rendered inert persona block to append to the system prompt,
    /// already fenced/framed as untrusted operator data by the gateway.
    /// `None` when the tenant has no persona configured (defaults apply).
    pub persona: Option<String>,
    /// Composio toolkit slugs (e.g. `"stripe"`, `"zendesk"`) this tenant
    /// currently has a live connected account for, resolved by the gateway
    /// from a synced `product.agent_toolkit_connections` table — never from
    /// agent/LLM output. Consumed by
    /// `Config::mcp_servers_for_agent_and_tenant` /
    /// `apply_toolkit_connection_gate` (`zeroclaw-config`) to drop any
    /// `[[mcp.servers]]` entry whose `requires_composio_toolkit` slug isn't
    /// in this list — Aivory's default toolkit (OfficeCLI, the native n8n
    /// bridge) is never gated by this and stays granted regardless; only
    /// external, tenant-owned-account toolkits are. Empty (not resolution
    /// failure) both when the tenant genuinely has no connections and when
    /// the connection-status lookup itself failed — that resolution is
    /// deliberately fail-open on turn availability (a DB hiccup here must
    /// never block memory/persona/native-tool access) but fail-closed on
    /// the grant itself (never over-grant on an inconclusive read).
    pub connected_toolkits: Vec<String>,
}

tokio::task_local! {
    /// Tenant overlay for the current turn. Scoped by the gateway around
    /// the `process_message` future; `None`/unset everywhere else.
    pub static TENANT_CONTEXT: Option<Arc<TenantContext>>;
}

/// The tenant overlay for the current task, if any.
///
/// Returns `None` both when running outside a scoped turn (vanilla
/// single-operator paths) and when the turn was scoped with `None`.
pub fn current_tenant() -> Option<Arc<TenantContext>> {
    TENANT_CONTEXT.try_with(std::clone::Clone::clone).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_context_reads_none() {
        assert!(current_tenant().is_none());
    }

    #[tokio::test]
    async fn scoped_context_is_visible_inside_and_gone_outside() {
        let ctx = Arc::new(TenantContext {
            tenant_id: "u1:cs".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "customer_service".to_string(),
            persona: Some("<operator_config>…</operator_config>".to_string()),
            connected_toolkits: Vec::new(),
        });
        TENANT_CONTEXT
            .scope(Some(ctx.clone()), async {
                let seen = current_tenant().expect("tenant visible inside scope");
                assert_eq!(seen.tenant_id, "u1:cs");
            })
            .await;
        assert!(current_tenant().is_none());
    }
}
