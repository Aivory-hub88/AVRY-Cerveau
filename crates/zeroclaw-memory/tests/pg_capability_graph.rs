//! Cerveau Phase 4.2: integration tests for the Postgres-backed capability
//! graph (`zeroclaw_memory::capability_graph`).
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set (CI
//! provides a pgvector service container, same as `pg_lifecycle.rs`). Absent
//! that env var the test is a no-op so the suite stays green without
//! Postgres.
//!
//! Proven: co-activation writes create/strengthen an undirected edge;
//! rerank boosts a candidate with a learned edge to something recently
//! activated above one with none; two tenants' edges never influence each
//! other's ranking (the actual isolation guarantee this feature must hold).

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::capability_graph::{CapabilityGraphRanker, PgCapabilityGraph};

const SCHEMA: &str = "cerveau_capgraph_test";

fn pg_url() -> Option<String> {
    std::env::var("CERVEAU_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

async fn exec(sql: &str) {
    let url = pg_url().unwrap();
    let sql = sql.to_string();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        c.batch_execute(&sql).expect("admin exec");
    })
    .await
    .expect("admin task join");
}

#[tokio::test(flavor = "multi_thread")]
async fn capability_graph_end_to_end() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping capability graph test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;

    let graph = PgCapabilityGraph::connect(&url, SCHEMA)
        .await
        .expect("connect + init schema");

    // ── Scenario 1: a fresh tenant with no history reranks to a no-op ──
    let candidates = vec!["srv__b".to_string(), "srv__a".to_string()];
    let ranked = graph
        .rerank("tenant-a", &candidates, &["srv__x".to_string()])
        .await;
    assert_eq!(
        ranked, candidates,
        "no edges yet ⇒ candidates pass through unranked"
    );

    // ── Scenario 2: co-activation creates a learned edge, reranking boosts it ──
    graph
        .record_co_activation(
            "tenant-a",
            &["srv__x".to_string(), "srv__b".to_string()],
        )
        .await;
    let ranked = graph
        .rerank(
            "tenant-a",
            &["srv__a".to_string(), "srv__b".to_string()],
            &["srv__x".to_string()],
        )
        .await;
    assert_eq!(
        ranked,
        vec!["srv__b".to_string(), "srv__a".to_string()],
        "srv__b has a learned edge to the recently-activated srv__x and must rank first"
    );

    // ── Scenario 3: repeated co-activation strengthens the edge (doesn't error/duplicate) ──
    graph
        .record_co_activation("tenant-a", &["srv__x".to_string(), "srv__b".to_string()])
        .await;
    let row_count: i64 = {
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let row = c
                .query_one(
                    &format!(
                        "SELECT weight FROM {SCHEMA}.kg_capability_edges \
                         WHERE tenant_id = 'tenant-a' AND tool_a = 'srv__b' AND tool_b = 'srv__x'"
                    ),
                    &[],
                )
                .unwrap();
            row.get::<_, f64>(0) as i64
        })
        .await
        .unwrap()
    };
    assert_eq!(
        row_count, 2,
        "second co-activation must strengthen the existing edge (weight=2), not duplicate the row"
    );

    // ── Scenario 4: tenant isolation — tenant B's identical query sees no boost ──
    let ranked_b = graph
        .rerank(
            "tenant-b",
            &["srv__a".to_string(), "srv__b".to_string()],
            &["srv__x".to_string()],
        )
        .await;
    assert_eq!(
        ranked_b,
        vec!["srv__a".to_string(), "srv__b".to_string()],
        "tenant-a's learned edge must never influence tenant-b's ranking"
    );

    // ── Scenario 5: single-name and empty co-activation calls are no-ops, not errors ──
    graph.record_co_activation("tenant-a", &[]).await;
    graph
        .record_co_activation("tenant-a", &["srv__solo".to_string()])
        .await;

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;")).await;
}
