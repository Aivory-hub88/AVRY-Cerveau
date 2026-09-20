//! Cerveau ADR-016 P1: the Postgres memory backend now stores and reads `importance`, can mark
//! memories as superseded, and counts recall hits.
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set (CI provides a pgvector
//! service container). Keyword-only (no embedder), so the pgvector extension is not needed.
//!
//! One test on one scratch schema, scenarios in sequence, like `pg_lifecycle`.

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::postgres::{PgLifecycleConfig, PostgresMemory};
use zeroclaw_memory::{Memory, MemoryCategory, MemoryEntry};

const SCHEMA: &str = "cerveau_p1";

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

/// First column of every row, as text.
async fn column(sql: &str) -> Vec<String> {
    let url = pg_url().unwrap();
    let sql = sql.to_string();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        c.query(&sql, &[])
            .expect("admin query")
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    })
    .await
    .expect("admin task join")
}

async fn store(
    mem: &PostgresMemory,
    agent: &str,
    key: &str,
    content: &str,
    category: MemoryCategory,
    importance: Option<f64>,
) {
    let uuid = mem.ensure_agent_uuid(agent).await.expect("uuid");
    mem.store_with_agent(key, content, category, None, None, importance, Some(&uuid))
        .await
        .expect("store");
}

async fn all(mem: &PostgresMemory, agent: &str) -> Vec<MemoryEntry> {
    let uuid = mem.ensure_agent_uuid(agent).await.expect("uuid");
    mem.recall_for_agents(&[&uuid], "*", 1000, None, None, None)
        .await
        .expect("recall")
}

async fn keys(mem: &PostgresMemory, agent: &str) -> Vec<String> {
    let mut k: Vec<String> = all(mem, agent).await.into_iter().map(|e| e.key).collect();
    k.sort();
    k
}

fn imp(entries: &[MemoryEntry], key: &str) -> Option<f64> {
    entries
        .iter()
        .find(|e| e.key == key)
        .unwrap_or_else(|| panic!("{key} not returned"))
        .importance
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-4)
}

fn open() -> PostgresMemory {
    PostgresMemory::new(
        "p1",
        &pg_url().unwrap(),
        SCHEMA,
        "memories",
        Some(5),
        Some(false),
        None,
        None,
        0.7,
        0.3,
    )
    .expect("connect + migrate")
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_memory_p1_end_to_end() {
    if pg_url().is_none() {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping P1 memory test");
        return;
    }
    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;
    let mem = open();
    mem.init_lifecycle_schema().await.expect("lifecycle schema");

    // ── 1. Importance is stored, clamped, defaulted and read back ─────────
    store(
        &mem,
        "t",
        "explicit",
        "standing rule",
        MemoryCategory::Core,
        Some(0.9),
    )
    .await;
    store(&mem, "t", "clamped", "x", MemoryCategory::Core, Some(4.2)).await;
    store(
        &mem,
        "t",
        "heuristic_core",
        "some fact",
        MemoryCategory::Core,
        None,
    )
    .await;
    store(
        &mem,
        "t",
        "heuristic_daily",
        "some note",
        MemoryCategory::Daily,
        None,
    )
    .await;
    store(
        &mem,
        "t",
        "boosted",
        "this one is critical",
        MemoryCategory::Core,
        None,
    )
    .await;
    let rows = all(&mem, "t").await;
    assert!(close(imp(&rows, "explicit"), 0.9), "explicit kept");
    assert!(close(imp(&rows, "clamped"), 1.0), "explicit clamped to 1");
    assert!(close(imp(&rows, "heuristic_core"), 0.7), "core base score");
    assert!(
        close(imp(&rows, "heuristic_daily"), 0.3),
        "daily base score"
    );
    assert!(close(imp(&rows, "boosted"), 0.8), "keyword boost applies");

    // Re-storing a key without an explicit value must not lower a deliberate one; a new
    // explicit value replaces it.
    store(
        &mem,
        "t",
        "explicit",
        "standing rule, reworded",
        MemoryCategory::Core,
        None,
    )
    .await;
    let rows = all(&mem, "t").await;
    assert!(
        close(imp(&rows, "explicit"), 0.9),
        "re-store keeps deliberate value"
    );
    store(
        &mem,
        "t",
        "explicit",
        "standing rule, reworded",
        MemoryCategory::Core,
        Some(0.3),
    )
    .await;
    let rows = all(&mem, "t").await;
    assert!(
        close(imp(&rows, "explicit"), 0.3),
        "explicit re-store replaces it"
    );

    // ── 2. Supersede: hidden from recall, reversible, revived by a fresh write ──
    exec(&format!("TRUNCATE {SCHEMA}.memories")).await;
    store(
        &mem,
        "s",
        "old_price",
        "price is 10",
        MemoryCategory::Core,
        None,
    )
    .await;
    store(
        &mem,
        "s",
        "new_price",
        "price is 12",
        MemoryCategory::Core,
        None,
    )
    .await;
    let rows = all(&mem, "s").await;
    let old_id = rows
        .iter()
        .find(|e| e.key == "old_price")
        .unwrap()
        .id
        .clone();
    let new_id = rows
        .iter()
        .find(|e| e.key == "new_price")
        .unwrap()
        .id
        .clone();
    assert_eq!(rows[0].superseded_by, None);

    assert_eq!(mem.mark_superseded(&[&old_id], &new_id).await.unwrap(), 1);
    assert_eq!(
        keys(&mem, "s").await,
        ["new_price"],
        "superseded row hidden"
    );
    let keyword = mem
        .recall_for_agents(
            &[&mem.ensure_agent_uuid("s").await.unwrap()],
            "price",
            10,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(keyword.len(), 1, "hidden from keyword recall too");
    // A row cannot supersede itself.
    assert_eq!(mem.mark_superseded(&[&new_id], &new_id).await.unwrap(), 0);

    assert_eq!(mem.clear_superseded(&[&old_id]).await.unwrap(), 1);
    assert_eq!(
        keys(&mem, "s").await,
        ["new_price", "old_price"],
        "reversible"
    );

    mem.mark_superseded(&[&old_id], &new_id).await.unwrap();
    store(
        &mem,
        "s",
        "old_price",
        "price is 10 again",
        MemoryCategory::Core,
        None,
    )
    .await;
    assert_eq!(
        keys(&mem, "s").await,
        ["new_price", "old_price"],
        "re-storing the key revives it"
    );

    // ── 3. Budget: importance decides, superseded rows go first ───────────
    exec(&format!("TRUNCATE {SCHEMA}.memories")).await;
    let cap2 = PgLifecycleConfig {
        conversation_retention_days: None,
        daily_retention_days: None,
        core_max_rows_per_tenant: 2,
        daily_max_rows_per_tenant: 2,
        conversation_max_rows_per_tenant: 2,
    };
    store(
        &mem,
        "b",
        "important_old",
        "rule",
        MemoryCategory::Daily,
        Some(0.9),
    )
    .await;
    store(
        &mem,
        "b",
        "low_mid",
        "note",
        MemoryCategory::Daily,
        Some(0.1),
    )
    .await;
    store(
        &mem,
        "b",
        "low_new",
        "note",
        MemoryCategory::Daily,
        Some(0.1),
    )
    .await;
    exec(&format!(
        "UPDATE {SCHEMA}.memories SET created_at = now() - interval '90 days' WHERE key = 'important_old';
         UPDATE {SCHEMA}.memories SET created_at = now() - interval '10 days' WHERE key = 'low_mid';"
    ))
    .await;
    let report = mem.run_lifecycle(&cap2).await.unwrap();
    assert_eq!(report.budget_evicted, 1);
    assert_eq!(
        keys(&mem, "b").await,
        ["important_old", "low_new"],
        "the old but important row survives; the older low-importance one is evicted"
    );

    // A superseded row is evicted before a live one, whatever its importance.
    let rows = all(&mem, "b").await;
    let dead = rows
        .iter()
        .find(|e| e.key == "important_old")
        .unwrap()
        .id
        .clone();
    let live = rows.iter().find(|e| e.key == "low_new").unwrap().id.clone();
    mem.mark_superseded(&[&dead], &live).await.unwrap();
    store(&mem, "b", "extra", "note", MemoryCategory::Daily, Some(0.1)).await;
    // `important_old` is hidden from recall now, so count in SQL.
    mem.run_lifecycle(&cap2).await.unwrap();
    let left = column(&format!("SELECT key FROM {SCHEMA}.memories ORDER BY key")).await;
    assert_eq!(left, ["extra", "low_new"], "superseded row evicted first");

    // ── 4. Access tracking counts recall hits ─────────────────────────────
    exec(&format!("TRUNCATE {SCHEMA}.memories")).await;
    store(&mem, "a", "seen", "alpha beta", MemoryCategory::Core, None).await;
    store(
        &mem,
        "a",
        "unseen",
        "gamma delta",
        MemoryCategory::Core,
        None,
    )
    .await;
    let uuid = mem.ensure_agent_uuid("a").await.unwrap();
    for _ in 0..3 {
        let hits = mem
            .recall_for_agents(&[&uuid], "alpha", 10, None, None, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }
    let counts = column(&format!(
        "SELECT key || '=' || access_count || '/' || (last_accessed_at IS NOT NULL) \
         FROM {SCHEMA}.memories ORDER BY key"
    ))
    .await;
    assert_eq!(counts, ["seen=3/true", "unseen=0/false"]);

    // ── 5. A table created before P1 upgrades in place ────────────────────
    drop(mem);
    exec(&format!(
        "ALTER TABLE {SCHEMA}.memories DROP COLUMN importance, DROP COLUMN superseded_by, \
         DROP COLUMN access_count, DROP COLUMN last_accessed_at"
    ))
    .await;
    let mem = open();
    let cols = column(&format!(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = '{SCHEMA}' AND table_name = 'memories' \
         AND column_name IN ('importance','superseded_by','access_count','last_accessed_at') \
         ORDER BY column_name"
    ))
    .await;
    assert_eq!(
        cols,
        [
            "access_count",
            "importance",
            "last_accessed_at",
            "superseded_by"
        ]
    );
    // Rows written before the upgrade read back with no importance and a zero count.
    let rows = all(&mem, "a").await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|e| e.importance.is_none()));
}
