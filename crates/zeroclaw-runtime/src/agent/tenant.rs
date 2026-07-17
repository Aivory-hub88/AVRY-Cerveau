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
    /// Pre-rendered inert persona block to append to the system prompt,
    /// already fenced/framed as untrusted operator data by the gateway.
    /// `None` when the tenant has no persona configured (defaults apply).
    pub persona: Option<String>,
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
            persona: Some("<operator_config>…</operator_config>".to_string()),
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
