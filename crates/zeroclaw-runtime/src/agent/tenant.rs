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
    /// Cerveau (Part C, per-agent tool-scope toggle): Composio toolkit
    /// slugs this tenant has explicitly *disabled* via the dashboard's
    /// Tools tab (`avry-backend`'s `product.agent_tool_scope`, `enabled =
    /// false` rows), resolved by the gateway. Consumed by
    /// `Config::mcp_servers_for_agent_and_tenant` /
    /// `apply_toolkit_scope_gate` (`zeroclaw-config`).
    ///
    /// **Opposite fail-direction from `connected_toolkits` above**: this
    /// list is a denylist, not an allowlist — a server is granted UNLESS
    /// its slug appears here, matching the dashboard's own "default
    /// enabled" contract. Empty means "the tenant hasn't disabled
    /// anything" in both the genuine case and the resolution-failed case
    /// (fail-open on availability, same reasoning as `connected_toolkits`,
    /// but here fail-open on availability also happens to be fail-open on
    /// the grant — an inconclusive read here can only ever under-restrict,
    /// never over-grant beyond what was already unconditionally true
    /// before this feature existed).
    pub disabled_toolkits: Vec<String>,
    /// ADR-006 Part B: this tenant's own registered MCP servers
    /// (`avry-backend`'s `product.tenant_custom_mcp_servers`, `status =
    /// 'verified'` rows only), resolved by the gateway's
    /// `TenantCustomMcpResolver` over HTTP (not direct SQL — the AES key
    /// that decrypts `auth_header_value` lives in exactly one process,
    /// avry-backend). Empty in both the genuine "tenant has none
    /// registered" case and the resolution-failed case — same fail-open-
    /// on-availability posture as `connected_toolkits`/`disabled_toolkits`
    /// above: an unreachable avry-backend must never block memory/persona/
    /// native-tool access, and can only ever under-grant here (never
    /// silently add a server that wasn't actually resolved).
    ///
    /// Turned into real `McpServerConfig` entries by
    /// [`Self::custom_mcp_server_configs`] and appended (not gated — these
    /// bypass the Composio-specific `apply_toolkit_connection_gate`/
    /// `apply_toolkit_scope_gate` chain entirely, since they're not
    /// Composio-sourced) in `Config::mcp_servers_for_agent_and_tenant`.
    pub tenant_custom_mcp_servers: Vec<TenantCustomMcpServer>,
}

/// One tenant-registered MCP server, already decrypted and verified —
/// exactly what avry-backend's internal
/// `GET /api/v1/tenant-mcp-servers/internal/{user_id}/{agent_type}` route
/// returns per entry. See [`TenantContext::tenant_custom_mcp_servers`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TenantCustomMcpServer {
    pub name: String,
    pub url: String,
    /// `"streamable-http"` or `"sse"` — matches
    /// `product.tenant_custom_mcp_servers.transport`'s two allowed values.
    pub transport: String,
    pub auth_header_name: Option<String>,
    pub auth_header_value: Option<String>,
    /// Always `"irreversible"` today (ADR-006 §B5: admin-only override,
    /// never tenant-set) — carried through rather than hardcoded here so a
    /// future Aivory-admin override doesn't require a wire-format change,
    /// though nothing currently sets it to anything else.
    pub risk_tier: String,
}

/// Tool-name prefix a tenant custom MCP server's tools carry, distinct from
/// any curated (`[[mcp.servers]]`) server name — both the collision-safety
/// margin `TenantContext::custom_mcp_server_configs` relies on and the
/// non-bypassable marker `TenantContext::is_tenant_custom_mcp_tool` checks
/// for. No curated Aivory server uses this prefix.
const TENANT_CUSTOM_MCP_NAME_PREFIX: &str = "tenant_";

impl TenantContext {
    /// Synthesizes a real `McpServerConfig` per registered server, always
    /// with `guarded_transport: true` (SSRF-guarded DNS-pinned transport —
    /// see `zeroclaw_tools::guarded_resolve`'s module docs) and a name
    /// prefixed with [`TENANT_CUSTOM_MCP_NAME_PREFIX`] so the resulting
    /// `<server>__<tool>` tool names are both visually unmistakable as
    /// tenant-supplied in logs/traces and structurally distinct from any
    /// curated server name — never set directly from `[[mcp.servers]]`, so
    /// no config collision is possible either.
    pub fn custom_mcp_server_configs(&self) -> Vec<zeroclaw_config::schema::McpServerConfig> {
        use zeroclaw_config::schema::{McpServerConfig, McpTransport};

        self.tenant_custom_mcp_servers
            .iter()
            .map(|server| {
                let mut headers = std::collections::HashMap::new();
                if let (Some(name), Some(value)) =
                    (&server.auth_header_name, &server.auth_header_value)
                {
                    headers.insert(name.clone(), value.clone());
                }
                McpServerConfig {
                    name: format!("{TENANT_CUSTOM_MCP_NAME_PREFIX}{}", server.name),
                    transport: if server.transport == "sse" {
                        McpTransport::Sse
                    } else {
                        McpTransport::Http
                    },
                    url: Some(server.url.clone()),
                    headers,
                    // ADR-006 §B4 item 7: bounded per-call runtime, independent
                    // of whatever a curated server's own config allows — a
                    // tenant-supplied tool must not consume the turn's whole
                    // budget. `guarded_resolve::GUARDED_TOTAL_TIMEOUT` (30s)
                    // already enforces this at the transport layer; this is
                    // belt-and-suspenders at the MCP-client-timeout layer too.
                    tool_timeout_secs: Some(30),
                    guarded_transport: true,
                    ..Default::default()
                }
            })
            .collect()
    }

    /// True if `tool_name` originates from one of this tenant's own custom
    /// MCP servers — the non-bypassable check
    /// `crate::approval::ApprovalManager::risk_tier` consults *before*
    /// anything in `[tool_risk_tiers]`, so no config entry can ever
    /// downgrade a tenant-supplied tool below `Irreversible` (ADR-006 §B5).
    pub fn is_tenant_custom_mcp_tool(&self, tool_name: &str) -> bool {
        self.tenant_custom_mcp_servers.iter().any(|server| {
            tool_name.starts_with(&format!("{TENANT_CUSTOM_MCP_NAME_PREFIX}{}__", server.name))
        })
    }
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
    TENANT_CONTEXT
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten()
}

/// Cerveau (F-1-for-approvals, patch 0028): what a later, out-of-band
/// resume of a `Pending` approval needs to react to "what did the user
/// actually ask", captured once at turn start.
///
/// `process_message` rebuilds `history` from scratch on every call — a
/// webhook-driven tenant turn has no literal saved transcript to pull this
/// back out of later (continuity today comes only from memory recall
/// scoped by `session_id`, never from replaying prior messages). So a
/// resume path that wants to synthesize a coherent continuation prompt
/// (mirroring `control_plane::continuation_drive`'s established shape)
/// must have the original message captured up front, not reconstructed
/// after the fact — there is nothing to reconstruct it from.
#[derive(Debug, Clone)]
pub struct TurnOriginContext {
    /// Session id, when the caller supplied one — threaded into a resumed
    /// turn so its memory recall sees the same tenant facts the original
    /// turn did. `None` for a turn with no session scoping.
    pub session_id: Option<String>,
    /// Verbatim user message that started this turn.
    pub origin_message: String,
}

tokio::task_local! {
    /// Origin context for the current turn. Scoped by the gateway
    /// alongside [`TENANT_CONTEXT`]; `None`/unset everywhere else.
    pub static TURN_ORIGIN_CONTEXT: Option<Arc<TurnOriginContext>>;
}

/// The turn-origin context for the current task, if any.
pub fn current_turn_origin() -> Option<Arc<TurnOriginContext>> {
    TURN_ORIGIN_CONTEXT
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten()
}

/// Cerveau (patch 0035): a structured, wire-shape-stable summary of the
/// `Pending` approval a turn created, if any — what a channel front-end
/// (e.g. `avry-backend`'s Telegram inline-approval buttons) needs to attach
/// an "Approve"/"Deny" affordance to a reply, without having to scrape the
/// id back out of the model's own free-text response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingApprovalSummary {
    pub id: String,
    pub tool_name: String,
    pub risk_tier: String,
}

tokio::task_local! {
    /// Write side of [`PendingApprovalSummary`] plumbing. Unlike
    /// `TENANT_CONTEXT`/`TURN_ORIGIN_CONTEXT` (set once at scope entry,
    /// read-only thereafter), this cell starts empty and is written to
    /// *during* the turn by [`record_pending_approval`] (called from deep
    /// inside `approval_gate::gate_tool_approval`, the only place a
    /// `Pending` row is ever created) — hence the `Mutex`-wrapped interior
    /// mutability rather than a plain `Option<Arc<T>>`. `None` when unset
    /// (no scope) or never written to during a vanilla/non-tenant turn.
    pub static LAST_PENDING_APPROVAL: Option<Arc<parking_lot::Mutex<Option<PendingApprovalSummary>>>>;
}

/// Record that this turn's tool call was gated into a durable `Pending`
/// approval — a no-op (not an error) when the current task was never
/// scoped with `LAST_PENDING_APPROVAL`, so this is safe to call
/// unconditionally from `gate_tool_approval` regardless of caller.
pub fn record_pending_approval(summary: PendingApprovalSummary) {
    let _ = LAST_PENDING_APPROVAL.try_with(|cell| {
        if let Some(cell) = cell {
            *cell.lock() = Some(summary);
        }
    });
}

/// Take (and clear) this turn's recorded pending-approval summary, if any.
/// "Take" rather than "get": a turn produces at most one HTTP response, so
/// the value is consumed exactly once by the caller that scoped it — a
/// second read after that (there shouldn't be one) sees `None`, not stale
/// data from a previous turn that happened to reuse the same task-local
/// slot.
pub fn take_pending_approval() -> Option<PendingApprovalSummary> {
    LAST_PENDING_APPROVAL
        .try_with(|cell| cell.as_ref().and_then(|cell| cell.lock().take()))
        .ok()
        .flatten()
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
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: Vec::new(),
        });
        TENANT_CONTEXT
            .scope(Some(ctx.clone()), async {
                let seen = current_tenant().expect("tenant visible inside scope");
                assert_eq!(seen.tenant_id, "u1:cs");
            })
            .await;
        assert!(current_tenant().is_none());
    }

    fn sample_custom_server() -> TenantCustomMcpServer {
        TenantCustomMcpServer {
            name: "orders".to_string(),
            url: "https://tenant.example/mcp".to_string(),
            transport: "streamable-http".to_string(),
            auth_header_name: Some("X-Api-Key".to_string()),
            auth_header_value: Some("secret123".to_string()),
            risk_tier: "irreversible".to_string(),
        }
    }

    #[test]
    fn custom_mcp_server_configs_sets_guarded_transport_and_prefixed_name() {
        let ctx = TenantContext {
            tenant_id: "u1:cs".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "customer_service".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: vec![sample_custom_server()],
        };
        let configs = ctx.custom_mcp_server_configs();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "tenant_orders");
        assert!(configs[0].guarded_transport);
        assert_eq!(
            configs[0].url.as_deref(),
            Some("https://tenant.example/mcp")
        );
        assert_eq!(
            configs[0].headers.get("X-Api-Key").map(String::as_str),
            Some("secret123")
        );
    }

    #[test]
    fn custom_mcp_server_configs_maps_sse_transport() {
        let mut server = sample_custom_server();
        server.transport = "sse".to_string();
        let ctx = TenantContext {
            tenant_id: "u1:cs".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "customer_service".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: vec![server],
        };
        assert_eq!(
            ctx.custom_mcp_server_configs()[0].transport,
            zeroclaw_config::schema::McpTransport::Sse
        );
    }

    #[test]
    fn custom_mcp_server_configs_omits_headers_when_no_auth_configured() {
        let mut server = sample_custom_server();
        server.auth_header_name = None;
        server.auth_header_value = None;
        let ctx = TenantContext {
            tenant_id: "u1:cs".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "customer_service".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: vec![server],
        };
        assert!(ctx.custom_mcp_server_configs()[0].headers.is_empty());
    }

    #[test]
    fn is_tenant_custom_mcp_tool_matches_own_prefixed_tools_only() {
        let ctx = TenantContext {
            tenant_id: "u1:cs".to_string(),
            platform_user_id: "u1".to_string(),
            agent_type: "customer_service".to_string(),
            persona: None,
            connected_toolkits: Vec::new(),
            disabled_toolkits: Vec::new(),
            tenant_custom_mcp_servers: vec![sample_custom_server()],
        };
        assert!(ctx.is_tenant_custom_mcp_tool("tenant_orders__get_order"));
        assert!(!ctx.is_tenant_custom_mcp_tool("tenant_other__get_order"));
        assert!(
            !ctx.is_tenant_custom_mcp_tool(
                "composio-zendesk-support__ZENDESK_REPLY_ZENDESK_TICKET"
            )
        );
        assert!(!ctx.is_tenant_custom_mcp_tool("shell"));
        // A tool name that merely starts with the server name but isn't
        // actually `<prefix><name>__` (no double-underscore boundary) must
        // not match — guards against a tenant naming their server e.g.
        // "orders_evil" and colliding with "tenant_orders__..." by prefix
        // alone.
        assert!(!ctx.is_tenant_custom_mcp_tool("tenant_orders_evil__whatever"));
    }

    #[tokio::test]
    async fn absent_turn_origin_reads_none() {
        assert!(current_turn_origin().is_none());
    }

    #[tokio::test]
    async fn scoped_turn_origin_is_visible_inside_and_gone_outside() {
        let ctx = Arc::new(TurnOriginContext {
            session_id: Some("sess-1".to_string()),
            origin_message: "please finalize invoice inv_123".to_string(),
        });
        TURN_ORIGIN_CONTEXT
            .scope(Some(ctx.clone()), async {
                let seen = current_turn_origin().expect("turn origin visible inside scope");
                assert_eq!(seen.session_id.as_deref(), Some("sess-1"));
                assert_eq!(seen.origin_message, "please finalize invoice inv_123");
            })
            .await;
        assert!(current_turn_origin().is_none());
    }

    #[tokio::test]
    async fn record_pending_approval_outside_any_scope_is_a_safe_noop() {
        // No panic, no error — just nothing recorded, since there's nowhere
        // to record it. Mirrors how a non-tenant/CLI turn never scopes this
        // at all, so `gate_tool_approval` can call this unconditionally.
        record_pending_approval(PendingApprovalSummary {
            id: "pa_1".to_string(),
            tool_name: "STRIPE_FINALIZE_INVOICE".to_string(),
            risk_tier: "irreversible".to_string(),
        });
        assert!(take_pending_approval().is_none());
    }

    #[tokio::test]
    async fn scoped_but_untouched_reads_none() {
        let cell = Arc::new(parking_lot::Mutex::new(None));
        LAST_PENDING_APPROVAL
            .scope(Some(cell), async {
                assert!(take_pending_approval().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn record_then_take_returns_it_once_and_clears() {
        let cell = Arc::new(parking_lot::Mutex::new(None));
        LAST_PENDING_APPROVAL
            .scope(Some(cell), async {
                record_pending_approval(PendingApprovalSummary {
                    id: "pa_42".to_string(),
                    tool_name: "STRIPE_FINALIZE_INVOICE".to_string(),
                    risk_tier: "irreversible".to_string(),
                });
                let seen = take_pending_approval().expect("recorded summary visible");
                assert_eq!(seen.id, "pa_42");
                assert_eq!(seen.tool_name, "STRIPE_FINALIZE_INVOICE");
                assert_eq!(seen.risk_tier, "irreversible");
                // Second read is empty — "take", not "get".
                assert!(take_pending_approval().is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn a_later_record_overwrites_an_earlier_unread_one() {
        // Concurrent-tool-call turns (patch 0026's 4.4) can in principle hit
        // this gate twice in one turn — last-write-wins is the correct,
        // simple behavior here: the HTTP response can only carry one
        // pending_approval, and the most recent block is the most relevant
        // one for a human resolving via the very next message.
        let cell = Arc::new(parking_lot::Mutex::new(None));
        LAST_PENDING_APPROVAL
            .scope(Some(cell), async {
                record_pending_approval(PendingApprovalSummary {
                    id: "pa_1".to_string(),
                    tool_name: "TOOL_A".to_string(),
                    risk_tier: "irreversible".to_string(),
                });
                record_pending_approval(PendingApprovalSummary {
                    id: "pa_2".to_string(),
                    tool_name: "TOOL_B".to_string(),
                    risk_tier: "irreversible".to_string(),
                });
                let seen = take_pending_approval().expect("recorded summary visible");
                assert_eq!(seen.id, "pa_2");
            })
            .await;
    }
}
