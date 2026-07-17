//! Cerveau P-isolation: adversarial cross-tenant memory isolation tests.
//!
//! These exercise the exact mechanism `create_memory_for_tenant` builds in
//! production — a shared SQL backend wrapped per tenant by
//! [`AgentScopedMemory`] with an **empty** cross-agent allowlist — and
//! assert, adversarially, that one tenant's rows can never surface in
//! another tenant's recalls: not by matching keywords, not by wildcard or
//! empty queries, not by shared session ids, and not by a caller-supplied
//! allowlist naming the victim tenant directly. The backend under test is
//! SQLite (same `(key, agent_id)` composite row model as Postgres); the
//! wrapper is backend-agnostic past that contract.

use std::sync::Arc;

use tempfile::TempDir;
use zeroclaw_memory::{AgentScopedMemory, Memory, MemoryCategory, SqliteMemory};

/// Shared install-wide backend + two structurally-jailed tenant scopes,
/// mirroring `create_memory_for_tenant` (namespaced `t_` id, empty
/// allowlist).
async fn two_tenants(
    workspace: &TempDir,
) -> (Arc<dyn Memory>, AgentScopedMemory, AgentScopedMemory, String) {
    let shared: Arc<dyn Memory> =
        Arc::new(SqliteMemory::new("sqlite", workspace.path()).expect("sqlite init"));
    let id_a = shared
        .ensure_agent_uuid("t_user_a.cs")
        .await
        .expect("uuid a");
    let id_b = shared
        .ensure_agent_uuid("t_user_b.cs")
        .await
        .expect("uuid b");
    assert_ne!(id_a, id_b, "distinct tenants must get distinct ids");
    let tenant_a = AgentScopedMemory::new(shared.clone(), id_a.clone(), Vec::new());
    let tenant_b = AgentScopedMemory::new(shared.clone(), id_b, Vec::new());
    (shared, tenant_a, tenant_b, id_a)
}

#[tokio::test]
async fn tenant_a_secret_is_invisible_to_tenant_b() {
    let ws = TempDir::new().unwrap();
    let (_shared, tenant_a, tenant_b, _) = two_tenants(&ws).await;

    tenant_a
        .store(
            "customer_note",
            "SECRET-ALPHA: Toko Melati owes invoice 4411",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

    // A sees its own row.
    let own = tenant_a.recall("SECRET-ALPHA", 10, None, None, None).await.unwrap();
    assert_eq!(own.len(), 1, "owner must recall its own memory");

    // B: exact keyword, wildcard, and empty (recent) queries all come back
    // empty — the row must not exist from B's point of view.
    for query in ["SECRET-ALPHA", "*", "", "invoice"] {
        let leaked = tenant_b.recall(query, 50, None, None, None).await.unwrap();
        assert!(
            leaked.is_empty(),
            "tenant B recall({query:?}) leaked {} row(s)",
            leaked.len()
        );
    }
}

#[tokio::test]
async fn shared_session_id_does_not_bridge_tenants() {
    // The bridge keys conversations by binding/session id; a malicious or
    // buggy caller reusing another tenant's session id must still see
    // nothing — session is a sub-scope, never an alternative to tenant.
    let ws = TempDir::new().unwrap();
    let (_shared, tenant_a, tenant_b, _) = two_tenants(&ws).await;

    tenant_a
        .store(
            "sess_note",
            "SECRET-BRAVO inside session-42",
            MemoryCategory::Conversation,
            Some("session-42"),
        )
        .await
        .unwrap();

    let leaked = tenant_b
        .recall("SECRET-BRAVO", 50, Some("session-42"), None, None)
        .await
        .unwrap();
    assert!(leaked.is_empty(), "shared session id must not bridge tenants");
}

#[tokio::test]
async fn caller_allowlist_cannot_widen_past_the_jail() {
    // Even a caller that somehow knows tenant A's storage id and passes it
    // explicitly must get nothing: the wrapper intersects caller allowlists
    // with the bound (empty) one.
    let ws = TempDir::new().unwrap();
    let (_shared, tenant_a, tenant_b, id_a) = two_tenants(&ws).await;

    tenant_a
        .store(
            "widen_probe",
            "SECRET-CHARLIE must stay private",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

    let leaked = tenant_b
        .recall_for_agents(
            &[id_a.as_str()],
            "SECRET-CHARLIE",
            50,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        leaked.is_empty(),
        "caller-supplied allowlist widened the tenant jail"
    );
}

#[tokio::test]
async fn exact_key_lookup_is_tenant_scoped() {
    let ws = TempDir::new().unwrap();
    let (_shared, tenant_a, tenant_b, _) = two_tenants(&ws).await;

    tenant_a
        .store("kb_entry", "SECRET-DELTA pricing sheet", MemoryCategory::Core, None)
        .await
        .unwrap();

    let stolen = tenant_b.get("kb_entry").await.unwrap();
    assert!(
        stolen.is_none(),
        "exact-key lookup crossed the tenant boundary"
    );
}

#[tokio::test]
async fn vanilla_default_agent_does_not_see_tenant_rows() {
    // The host install's own (non-tenant) scope must not surface tenant
    // data either — e.g. the Console/Assistant paths running on the same
    // daemon.
    let ws = TempDir::new().unwrap();
    let (shared, tenant_a, _tenant_b, _) = two_tenants(&ws).await;

    tenant_a
        .store("t_note", "SECRET-ECHO tenant-only", MemoryCategory::Core, None)
        .await
        .unwrap();

    let default_id = shared.ensure_agent_uuid("default").await.unwrap();
    let vanilla = AgentScopedMemory::new(shared.clone(), default_id, Vec::new());
    let leaked = vanilla.recall("SECRET-ECHO", 50, None, None, None).await.unwrap();
    assert!(
        leaked.is_empty(),
        "install-default agent scope saw tenant rows"
    );
}
