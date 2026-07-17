//! Cerveau Phase 3: integration tests for the tenant-scoped Postgres memory
//! lifecycle (ADR-004).
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set (CI
//! provides a pgvector service container). Absent that env var the test is a
//! no-op so the suite stays green without Postgres.
//!
//! All scenarios run inside ONE test on ONE freshly-recreated schema,
//! sequentially. That is deliberate: upstream's v3 migration checks
//! `pg_constraint.conname` WITHOUT scoping to the table's schema, so two
//! same-named `memories` tables in different schemas being migrated at once
//! interfere. Production has a single `cerveau` schema, so this never bites
//! there — but parallel per-schema tests would. One schema, serial scenarios,
//! truncate between them.
//!
//! Proven: retention prune (core durable, conversation aged out); per-tenant
//! budget enforced INDEPENDENTLY per tenant in a single set-based pass; a
//! per-tenant quota override beats the default cap.

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::postgres::{PgLifecycleConfig, PostgresMemory};
use zeroclaw_memory::{Memory, MemoryCategory};

const SCHEMA: &str = "cerveau_life";

fn pg_url() -> Option<String> {
    std::env::var("CERVEAU_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

fn exec(sql: &str) {
    let url = pg_url().unwrap();
    let sql = sql.to_string();
    let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
    c.batch_execute(&sql).expect("admin exec");
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
    mem.recall_for_agents(&[agent], "*", 10_000, None, None, None)
        .await
        .expect("recall")
        .len()
}

fn truncate() {
    exec(&format!(
        "TRUNCATE {SCHEMA}.memories; TRUNCATE {SCHEMA}.cerveau_tenant_quota;"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_lifecycle_end_to_end() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping Postgres lifecycle test");
        return;
    };

    // Fresh schema so the v3 migration runs cleanly (see module docs).
    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ));

    let mem = PostgresMemory::new("test", &url, SCHEMA, "memories", Some(5), Some(false), None)
        .expect("connect + migrate");
    mem.init_lifecycle_schema().await.expect("init lifecycle");

    // ── Scenario 1: budget enforced independently per tenant ──────────
    seed(&mem, "tenant_a", MemoryCategory::Core, 20, "c").await;
    seed(&mem, "tenant_b", MemoryCategory::Core, 3, "c").await;
    let cap5 = PgLifecycleConfig {
        conversation_retention_days: None,
        daily_retention_days: None,
        core_max_rows_per_tenant: 5,
        daily_max_rows_per_tenant: 5,
        conversation_max_rows_per_tenant: 5,
    };
    let report = mem.run_lifecycle(&cap5).await.expect("lifecycle 1");
    assert_eq!(report.budget_evicted, 15, "A 20->5 evicts 15; B under cap");
    assert_eq!(count_for(&mem, "tenant_a").await, 5, "A capped at 5");
    assert_eq!(count_for(&mem, "tenant_b").await, 3, "B under cap untouched");

    // ── Scenario 2: per-tenant quota override beats the default ───────
    truncate();
    seed(&mem, "vip", MemoryCategory::Core, 20, "c").await;
    let vip_id = mem.ensure_agent_uuid("vip").await.expect("uuid");
    mem.set_tenant_quota(&vip_id, "core", 12).await.expect("quota");
    mem.run_lifecycle(&cap5).await.expect("lifecycle 2");
    assert_eq!(
        count_for(&mem, "vip").await,
        12,
        "override cap 12 wins over default 5"
    );

    // ── Scenario 3: core is durable, conversation is age-pruned ───────
    truncate();
    seed(&mem, "t", MemoryCategory::Core, 3, "core").await;
    seed(&mem, "t", MemoryCategory::Conversation, 3, "conv").await;
    exec(&format!(
        "UPDATE {SCHEMA}.memories SET created_at = now() - interval '400 days';"
    ));
    let with_retention = PgLifecycleConfig {
        conversation_retention_days: Some(30),
        daily_retention_days: Some(180),
        core_max_rows_per_tenant: 10_000,
        daily_max_rows_per_tenant: 10_000,
        conversation_max_rows_per_tenant: 10_000,
    };
    let report = mem.run_lifecycle(&with_retention).await.expect("lifecycle 3");
    assert_eq!(report.retention_pruned, 3, "3 aged conversation rows pruned");
    assert_eq!(count_for(&mem, "t").await, 3, "3 core rows survive (durable)");

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;"));
}
