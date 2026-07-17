//! Cerveau Phase 3: integration tests for the tenant-scoped Postgres memory
//! lifecycle (ADR-004).
//!
//! These require a live Postgres. They run only when `CERVEAU_TEST_PG_URL` is
//! set (a throwaway DB — the test creates/drops its own schema); absent that
//! env var every test is a no-op so the suite stays green on machines without
//! Postgres. CI sets it against a `postgres` service container.
//!
//! What they prove:
//! - retention prune removes rows older than the category age cap, leaves
//!   `core` untouched;
//! - per-tenant budget keeps the top-N per agent and is enforced INDEPENDENTLY
//!   per tenant in a single pass (tenant A over quota does not affect B);
//! - a per-tenant quota override beats the default cap.

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::postgres::{PgLifecycleConfig, PostgresMemory};
use zeroclaw_memory::{Memory, MemoryCategory};

fn pg_url() -> Option<String> {
    std::env::var("CERVEAU_TEST_PG_URL").ok().filter(|s| !s.is_empty())
}

/// Fresh backend on a uniquely-named schema so parallel test runs don't
/// collide; caller drops nothing (throwaway DB is discarded by CI).
async fn backend(schema: &str) -> PostgresMemory {
    let url = pg_url().expect("CERVEAU_TEST_PG_URL");
    let mem = PostgresMemory::new("test", &url, schema, "memories", Some(5), Some(false), None)
        .expect("connect");
    mem.init_lifecycle_schema().await.expect("init lifecycle schema");
    mem
}

async fn seed(mem: &PostgresMemory, agent: &str, category: MemoryCategory, n: usize, prefix: &str) {
    for i in 0..n {
        mem.store_with_agent(
            &format!("{prefix}_{agent}_{i}"),
            &format!("memory {prefix} {i} for {agent}"),
            category.clone(),
            None,
            Some(agent),
            None,
            None,
        )
        .await
        .expect("store");
    }
}

async fn count_for(mem: &PostgresMemory, agent: &str) -> usize {
    // recall_for_agents with a wildcard returns this agent's rows only.
    mem.recall_for_agents(&[agent], "*", 10_000, None, None, None)
        .await
        .expect("recall")
        .len()
}

#[tokio::test]
async fn budget_is_enforced_per_tenant_independently() {
    let Some(_) = pg_url() else { return };
    let mem = backend("cerveau_life_budget").await;

    // Tenant A: 20 core rows; tenant B: 3 core rows. Cap = 5.
    seed(&mem, "tenant_a", MemoryCategory::Core, 20, "c").await;
    seed(&mem, "tenant_b", MemoryCategory::Core, 3, "c").await;

    let cfg = PgLifecycleConfig {
        conversation_retention_days: None,
        daily_retention_days: None,
        core_max_rows_per_tenant: 5,
        daily_max_rows_per_tenant: 5,
        conversation_max_rows_per_tenant: 5,
    };
    let report = mem.run_lifecycle(&cfg).await.expect("lifecycle");
    assert_eq!(report.budget_evicted, 15, "A: 20 -> 5 evicts 15; B under cap");

    assert_eq!(count_for(&mem, "tenant_a").await, 5, "A capped at 5");
    assert_eq!(count_for(&mem, "tenant_b").await, 3, "B under cap untouched");
}

#[tokio::test]
async fn per_tenant_quota_override_beats_default() {
    let Some(_) = pg_url() else { return };
    let mem = backend("cerveau_life_override").await;

    // Need the agent UUIDs the memories were attributed to, to key the quota.
    seed(&mem, "vip", MemoryCategory::Core, 20, "c").await;
    let vip_id = mem.ensure_agent_uuid("vip").await.expect("uuid");
    mem.set_tenant_quota(&vip_id, "core", 12).await.expect("quota");

    let cfg = PgLifecycleConfig {
        conversation_retention_days: None,
        daily_retention_days: None,
        core_max_rows_per_tenant: 5, // default would cut to 5...
        daily_max_rows_per_tenant: 5,
        conversation_max_rows_per_tenant: 5,
    };
    mem.run_lifecycle(&cfg).await.expect("lifecycle");
    // ...but the override keeps 12.
    assert_eq!(count_for(&mem, "vip").await, 12, "override cap 12 wins over default 5");
}

#[tokio::test]
async fn core_is_never_age_pruned_but_conversation_is() {
    let Some(_) = pg_url() else { return };
    let mem = backend("cerveau_life_retention").await;

    seed(&mem, "t", MemoryCategory::Core, 3, "core").await;
    seed(&mem, "t", MemoryCategory::Conversation, 3, "conv").await;

    // Age every row well past the conversation cap.
    let url = pg_url().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        c.execute(
            "UPDATE cerveau_life_retention.memories SET created_at = now() - interval '400 days'",
            &[],
        )
        .unwrap();
    })
    .await
    .unwrap();

    let cfg = PgLifecycleConfig {
        conversation_retention_days: Some(30),
        daily_retention_days: Some(180),
        core_max_rows_per_tenant: 10_000,
        daily_max_rows_per_tenant: 10_000,
        conversation_max_rows_per_tenant: 10_000,
    };
    let report = mem.run_lifecycle(&cfg).await.expect("lifecycle");
    assert_eq!(report.retention_pruned, 3, "3 old conversation rows pruned");
    assert_eq!(count_for(&mem, "t").await, 3, "3 core rows survive (durable)");
}
