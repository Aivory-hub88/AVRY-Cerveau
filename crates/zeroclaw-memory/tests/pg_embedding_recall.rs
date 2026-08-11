//! Cerveau: integration tests for the Postgres memory backend's embedding
//! wiring — previously entirely absent (the `embedding vector(N)` column
//! existed via `try_enable_pgvector`, but nothing ever computed or wrote a
//! value into it, and `recall`/`recall_for_agents` never referenced it).
//! Found while verifying `vector_enabled = true` in production after
//! installing the `pgvector` extension: a store+recall round-trip using
//! paraphrased, zero-keyword-overlap text still "worked" — but only
//! because the row also happened to share other keywords, not because
//! semantic search was actually running. These tests prove the real
//! mechanism with a deterministic fake embedder, not an LLM-dependent one.
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

const SCHEMA: &str = "cerveau_embed_recall";

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

/// Deterministic, network-free embedder: a small lookup table of known
/// inputs to hand-picked 3-dimensional vectors, so a test can assert exact
/// ranking/inclusion behavior without depending on any real embedding
/// model's actual semantics. Unknown inputs map to a neutral zero vector
/// (orthogonal-ish to everything, never the top match).
struct FakeEmbedder;

#[async_trait]
impl EmbeddingProvider for FakeEmbedder {
    fn name(&self) -> &str {
        "fake"
    }

    fn dimensions(&self) -> usize {
        3
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| match *t {
                // "apple" (stored content) and "fruit" (paraphrased query,
                // zero keyword overlap with "apple") are close together.
                "apple" => vec![1.0, 0.0, 0.0],
                "fruit" => vec![0.9, 0.1, 0.0],
                // "shoe" is a keyword-and-semantic decoy, far from both.
                "shoe" => vec![0.0, 1.0, 0.0],
                _ => vec![0.0, 0.0, 0.0],
            })
            .collect())
    }
}

/// A Noop-shaped embedder that always errors — proves a broken embedder
/// degrades a store/recall call to keyword-only (logs, doesn't fail the
/// call), matching `SqliteMemory::store_row_with_metadata`'s established
/// posture, not `PostgresMemory`-specific behavior invented for this patch.
struct FailingEmbedder;

#[async_trait]
impl EmbeddingProvider for FailingEmbedder {
    fn name(&self) -> &str {
        "failing"
    }

    fn dimensions(&self) -> usize {
        3
    }

    async fn embed(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("embedding provider unreachable (simulated)")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_recall_finds_a_zero_keyword_overlap_match() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping embedding recall test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
    let mem = PostgresMemory::new(
        "test",
        &url,
        SCHEMA,
        "memories",
        Some(5),
        Some(true),
        Some(3),
        Some(embedder),
        0.7,
        0.3,
    )
    .expect("connect + migrate + enable pgvector");

    let uuid = mem.ensure_agent_uuid("tenant").await.expect("uuid");
    mem.store_with_agent("k_apple", "apple", MemoryCategory::Core, None, None, None, Some(&uuid))
        .await
        .expect("store apple");
    mem.store_with_agent("k_shoe", "shoe", MemoryCategory::Core, None, None, None, Some(&uuid))
        .await
        .expect("store shoe");

    // "fruit" shares ZERO keywords with "apple" or "shoe" — a pure keyword
    // search would return nothing for either. Only the embedder's learned
    // closeness (fruit ~ apple) should surface "apple", ranked above "shoe".
    let results = mem
        .recall_for_agents(&[&uuid], "fruit", 10, None, None, None)
        .await
        .expect("recall");

    assert!(
        !results.is_empty(),
        "semantic-only query ('fruit') must not be filtered out by the keyword WHERE clause \
         just because it shares no words with any stored content — this is the exact case \
         'hybrid' search exists for"
    );
    assert_eq!(
        results[0].key, "k_apple",
        "the row whose embedding is genuinely closest to the query embedding must rank first"
    );

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn embedding_column_is_actually_populated_on_store() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping embedding-populated test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA}_pop CASCADE; CREATE SCHEMA {SCHEMA}_pop;"
    ))
    .await;
    let schema = format!("{SCHEMA}_pop");

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
    let mem = PostgresMemory::new(
        "test", &url, &schema, "memories", Some(5), Some(true), Some(3), Some(embedder), 0.7, 0.3,
    )
    .expect("connect + migrate + enable pgvector");

    let uuid = mem.ensure_agent_uuid("tenant").await.expect("uuid");
    mem.store_with_agent("k_apple", "apple", MemoryCategory::Core, None, None, None, Some(&uuid))
        .await
        .expect("store apple");

    // Independent proof, not the model's/library's own claim: query the
    // raw column directly and confirm a real vector landed there — this is
    // the exact gap the whole patch exists to close (the column existed
    // but nothing ever wrote to it).
    let url_owned = url.clone();
    let schema_owned = schema.clone();
    let has_embedding: bool = tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url_owned, postgres::NoTls).expect("connect");
        let row = c
            .query_one(
                &format!("SELECT embedding IS NOT NULL AS has_it FROM {schema_owned}.memories WHERE key = 'k_apple'"),
                &[],
            )
            .expect("query");
        row.get("has_it")
    })
    .await
    .expect("join");

    assert!(
        has_embedding,
        "the embedding column must be populated after a store call with a real embedder configured"
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_embedder_stays_keyword_only_zero_overlap_finds_nothing() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping keyword-only regression test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA}_none CASCADE; CREATE SCHEMA {SCHEMA}_none;"
    ))
    .await;
    let schema = format!("{SCHEMA}_none");

    // No embedder at all (None) — must behave exactly like the pre-patch
    // keyword-only backend: a zero-keyword-overlap query finds nothing.
    let mem = PostgresMemory::new(
        "test", &url, &schema, "memories", Some(5), Some(true), Some(3), None, 0.7, 0.3,
    )
    .expect("connect + migrate + enable pgvector");

    let uuid = mem.ensure_agent_uuid("tenant").await.expect("uuid");
    mem.store_with_agent("k_apple", "apple", MemoryCategory::Core, None, None, None, Some(&uuid))
        .await
        .expect("store apple");

    let results = mem
        .recall_for_agents(&[&uuid], "fruit", 10, None, None, None)
        .await
        .expect("recall");

    assert!(
        results.is_empty(),
        "with no embedder configured, a query sharing no keywords with any stored content \
         must find nothing — regression check that the widened WHERE clause doesn't \
         accidentally return everything when there's no vector to actually rank by"
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_embedder_degrades_to_keyword_only_not_a_hard_error() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping failing-embedder test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA}_fail CASCADE; CREATE SCHEMA {SCHEMA}_fail;"
    ))
    .await;
    let schema = format!("{SCHEMA}_fail");

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FailingEmbedder);
    let mem = PostgresMemory::new(
        "test", &url, &schema, "memories", Some(5), Some(true), Some(3), Some(embedder), 0.7, 0.3,
    )
    .expect("connect + migrate + enable pgvector");

    let uuid = mem.ensure_agent_uuid("tenant").await.expect("uuid");

    // Store must succeed (not propagate the embed failure) — content is
    // persisted without a vector, matching SqliteMemory's own posture.
    let store_result = mem
        .store_with_agent(
            "k_apple",
            "apple pie recipe",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&uuid),
        )
        .await;
    assert!(
        store_result.is_ok(),
        "an embedding failure must not fail the store call: {:?}",
        store_result.err()
    );

    // Ordinary keyword recall must still work — a broken embedder degrades
    // vector search, it doesn't take down the whole backend.
    let results = mem
        .recall_for_agents(&[&uuid], "apple", 10, None, None, None)
        .await
        .expect("recall must still succeed on a keyword match");
    assert_eq!(results.len(), 1, "keyword recall must still work with a failing embedder");

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}
