//! Cerveau: a session-scoped Postgres recall must still see durable global
//! knowledge — `core`/`daily` rows stored with no session binding.
//!
//! Found in production: every webhook turn recalls with the chat's session
//! id, while long-term memories are stored session-less. The Postgres WHERE
//! clause matched `session_id = $2` only, so 234 of 234 recalls in the trace
//! returned zero rows although the table held ~1,000 core/daily memories.
//! SQLite already treats those rows as recallable from any session
//! (`DURABLE_GLOBAL_CATEGORIES`); this proves Postgres now does the same, on
//! both the keyword-only and the pgvector statement of `recall` and
//! `recall_for_agents`, without widening to other sessions' rows, session-less
//! conversation rows, or another tenant's rows.
//!
//! Requires a live Postgres with `CREATE EXTENSION vector` available (CI's
//! pgvector service container). Absent `CERVEAU_TEST_PG_URL` the test is a
//! no-op so the suite stays green without Postgres.

#![cfg(feature = "memory-postgres")]

use std::sync::Arc;

use async_trait::async_trait;
use zeroclaw_memory::embeddings::EmbeddingProvider;
use zeroclaw_memory::postgres::PostgresMemory;
use zeroclaw_memory::{Memory, MemoryCategory};

const SCHEMA: &str = "cerveau_session_recall";

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

/// Every text maps to the same vector, so on the pgvector statement every
/// row passes the vector half of the WHERE clause: the session filter is the
/// only thing deciding what comes back.
struct FlatEmbedder;

#[async_trait]
impl EmbeddingProvider for FlatEmbedder {
    fn name(&self) -> &str {
        "flat"
    }

    fn dimensions(&self) -> usize {
        3
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
}

async fn check(schema: &str, pgvector: bool) {
    let url = pg_url().unwrap();
    exec(&format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"
    ))
    .await;
    let embedder: Option<Arc<dyn EmbeddingProvider>> =
        pgvector.then(|| Arc::new(FlatEmbedder) as _);
    let mem = PostgresMemory::new(
        "test",
        &url,
        schema,
        "memories",
        Some(5),
        Some(pgvector),
        Some(3),
        embedder,
        0.7,
        0.3,
    )
    .expect("connect + migrate");

    let tenant = mem.ensure_agent_uuid("t_tenant").await.expect("uuid");
    let other = mem.ensure_agent_uuid("t_other").await.expect("uuid");
    let rows: [(&str, MemoryCategory, Option<&str>, &str); 6] = [
        ("k_core_global", MemoryCategory::Core, None, &tenant),
        ("k_daily_global", MemoryCategory::Daily, None, &tenant),
        (
            "k_same_session",
            MemoryCategory::Conversation,
            Some("chat-1"),
            &tenant,
        ),
        (
            "k_other_session",
            MemoryCategory::Core,
            Some("chat-2"),
            &tenant,
        ),
        (
            "k_conversation_global",
            MemoryCategory::Conversation,
            None,
            &tenant,
        ),
        ("k_other_tenant", MemoryCategory::Core, None, &other),
    ];
    for (key, category, session, agent) in rows {
        mem.store_with_agent(
            key,
            "invoice reminder",
            category,
            session,
            None,
            None,
            Some(agent),
        )
        .await
        .expect("store");
    }

    let keys = |entries: Vec<zeroclaw_memory::MemoryEntry>| {
        let mut k: Vec<String> = entries.into_iter().map(|e| e.key).collect();
        k.sort();
        k
    };
    let want = vec!["k_core_global", "k_daily_global", "k_same_session"];

    let scoped = mem
        .recall_for_agents(&[&tenant], "invoice", 50, Some("chat-1"), None, None)
        .await
        .expect("recall_for_agents");
    assert_eq!(
        keys(scoped),
        want,
        "pgvector={pgvector}: a session-scoped tenant recall returns its own session's rows plus \
         session-less core/daily rows — not another session's, not session-less conversation, \
         not another tenant's"
    );

    let all_agents = mem
        .recall("invoice", 50, Some("chat-1"), None, None)
        .await
        .expect("recall");
    let mut want_all = want.clone();
    want_all.push("k_other_tenant");
    want_all.sort();
    assert_eq!(
        keys(all_agents),
        want_all,
        "pgvector={pgvector}: unscoped-by-agent recall applies the same session rule"
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_scoped_recall_includes_durable_global_rows() {
    if pg_url().is_none() {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping session-scoped recall test");
        return;
    }
    check(SCHEMA, false).await;
    check(&format!("{SCHEMA}_vec"), true).await;
}
