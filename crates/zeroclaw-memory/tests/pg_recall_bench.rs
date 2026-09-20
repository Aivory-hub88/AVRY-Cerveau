//! Cerveau ADR-016 P0: recall-quality benchmark for the Postgres memory backend.
//!
//! Loads a synthetic, hand-written corpus (`fixtures/recall_bench/corpus.json`; English and
//! Indonesian, three agents, ages from 1 to 100 days) into a scratch schema, runs every query
//! through the real `recall_for_agents`, and reports hit@5 and MRR per query kind.
//!
//! Two pipelines are measured, because they answer different questions:
//!
//! * `raw`      what the backend returns (what the `memory_recall` tool sees);
//! * `injected` the same results after the production auto-injection filter: the flat 7-day
//!              time decay, then the `min_relevance_score = 0.4` floor (`memory_inject.rs`,
//!              live config 2026-09-20). This is what an agent sees without asking.
//!
//! Two modes, chosen by whether `fixtures/recall_bench/embeddings.json` exists:
//!
//! * `hybrid`  vectors are precomputed (same model and dimensions as production), so the run is
//!             deterministic, free and needs no network. Needs the pgvector extension.
//! * `keyword` no embeddings; keyword-only recall. The `injected` pipeline is not reported
//!             because keyword scores are not on the hybrid scale the 0.4 floor was set for.
//!
//! Runs only when `CERVEAU_TEST_PG_URL` is set. `RECALL_BENCH_WRITE_BASELINE=1` records the
//! current numbers as the baseline; otherwise the run fails if hit@5 falls more than the
//! tolerance below the recorded baseline for the same mode.
//!
//! The corpus is fictional. Do not add real tenant rows: this file is committed.

#![cfg(feature = "memory-postgres")]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroclaw_memory::decay::{DEFAULT_HALF_LIFE_DAYS, apply_time_decay};
use zeroclaw_memory::embeddings::EmbeddingProvider;
use zeroclaw_memory::postgres::PostgresMemory;
use zeroclaw_memory::{Memory, MemoryCategory};

const SCHEMA: &str = "cerveau_recall_bench";
const TOP_K: usize = 5;
/// Live `memory.min_relevance_score` (config.toml, 2026-09-20).
const MIN_RELEVANCE: f64 = 0.4;
/// Live `memory.vector_weight` / `keyword_weight`.
const VECTOR_WEIGHT: f32 = 0.7;
const KEYWORD_WEIGHT: f32 = 0.3;
/// hit@5 may drop by this much (absolute) before the run fails.
const TOLERANCE: f64 = 0.03;

#[derive(Deserialize)]
struct Corpus {
    memories: Vec<CorpusMemory>,
    queries: Vec<Query>,
}

#[derive(Deserialize)]
struct CorpusMemory {
    id: String,
    agent: String,
    category: String,
    age_days: i64,
    content: String,
}

#[derive(Deserialize)]
struct Query {
    id: String,
    kind: String,
    agent: String,
    query: String,
    expect: Vec<String>,
}

#[derive(Deserialize)]
struct Embeddings {
    dims: usize,
    vectors: HashMap<String, Vec<f32>>,
}

struct FixtureEmbedder {
    dims: usize,
    vectors: HashMap<String, Vec<f32>>,
}

#[async_trait]
impl EmbeddingProvider for FixtureEmbedder {
    fn name(&self) -> &str {
        "fixture"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|t| {
                self.vectors
                    .get(*t)
                    .cloned()
                    .ok_or_else(|| anyhow::Error::msg(format!("no fixture embedding for {t:?}")))
            })
            .collect()
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
struct Score {
    n: usize,
    hit: f64,
    mrr: f64,
}

/// mode -> pipeline -> kind ("all" included) -> score
type Report = BTreeMap<String, BTreeMap<String, BTreeMap<String, Score>>>;

fn pg_url() -> Option<String> {
    std::env::var("CERVEAU_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall_bench")
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

fn category(name: &str) -> MemoryCategory {
    match name {
        "core" => MemoryCategory::Core,
        "daily" => MemoryCategory::Daily,
        "conversation" => MemoryCategory::Conversation,
        other => panic!("unknown category {other}"),
    }
}

fn score(rankings: &[(Vec<String>, &Query)]) -> BTreeMap<String, Score> {
    let mut acc: BTreeMap<String, (usize, f64, f64)> = BTreeMap::new();
    for (ranked, q) in rankings {
        // Queries with no expected answer (negative) are checked for leaks elsewhere and
        // would only dilute hit@5 here.
        if q.expect.is_empty() {
            continue;
        }
        let rank = ranked.iter().take(TOP_K).position(|k| q.expect.contains(k));
        let (hit, rr) = match rank {
            Some(p) => (1.0, 1.0 / (p as f64 + 1.0)),
            None => (0.0, 0.0),
        };
        for key in [q.kind.as_str(), "all"] {
            let e = acc.entry(key.to_string()).or_default();
            e.0 += 1;
            e.1 += hit;
            e.2 += rr;
        }
    }
    acc.into_iter()
        .map(|(k, (n, h, r))| {
            (
                k,
                Score {
                    n,
                    hit: h / n as f64,
                    mrr: r / n as f64,
                },
            )
        })
        .collect()
}

fn print_table(mode: &str, pipeline: &str, scores: &BTreeMap<String, Score>) {
    eprintln!("[{mode}/{pipeline}]");
    for (kind, s) in scores {
        eprintln!(
            "  {kind:<11} n={:<3} hit@{TOP_K}={:.3} mrr={:.3}",
            s.n, s.hit, s.mrr
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_quality_baseline() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping recall benchmark");
        return;
    };

    let dir = fixtures();
    let corpus: Corpus =
        serde_json::from_slice(&std::fs::read(dir.join("corpus.json")).expect("corpus.json"))
            .expect("parse corpus");
    let embeddings: Option<Embeddings> = std::fs::read(dir.join("embeddings.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b).expect("parse embeddings.json"));
    let mode = if embeddings.is_some() {
        "hybrid"
    } else {
        "keyword"
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;

    let (embedder, dims): (Option<Arc<dyn EmbeddingProvider>>, Option<usize>) = match embeddings {
        Some(e) => {
            for t in corpus
                .memories
                .iter()
                .map(|m| &m.content)
                .chain(corpus.queries.iter().map(|q| &q.query))
            {
                assert!(
                    e.vectors.contains_key(t),
                    "embeddings.json is stale: missing {t:?}; rerun gen_embeddings.py"
                );
            }
            let dims = e.dims;
            (
                Some(Arc::new(FixtureEmbedder {
                    dims,
                    vectors: e.vectors,
                })),
                Some(dims),
            )
        }
        None => (None, None),
    };

    let mem = PostgresMemory::new(
        "bench",
        &url,
        SCHEMA,
        "memories",
        Some(5),
        Some(embedder.is_some()),
        dims,
        embedder,
        VECTOR_WEIGHT,
        KEYWORD_WEIGHT,
    )
    .expect("connect + migrate");

    let mut uuid_of: HashMap<String, String> = HashMap::new();
    let agents: std::collections::BTreeSet<&String> =
        corpus.memories.iter().map(|m| &m.agent).collect();
    for agent in agents {
        let id = mem.ensure_agent_uuid(agent).await.expect("agent uuid");
        uuid_of.insert(agent.clone(), id);
    }
    let agent_of_key: HashMap<&str, &str> = corpus
        .memories
        .iter()
        .map(|m| (m.id.as_str(), m.agent.as_str()))
        .collect();

    for m in &corpus.memories {
        mem.store_with_agent(
            &m.id,
            &m.content,
            category(&m.category),
            None,
            None,
            None,
            Some(&uuid_of[&m.agent]),
        )
        .await
        .expect("store");
        // Age the row: recall decay and the lifecycle both read these timestamps.
        exec(&format!(
            "UPDATE {SCHEMA}.memories SET created_at = now() - make_interval(days => {d}), \
             updated_at = now() - make_interval(days => {d}) WHERE key = '{k}'",
            d = m.age_days,
            k = m.id
        ))
        .await;
    }

    let mut raw: Vec<(Vec<String>, &Query)> = Vec::new();
    let mut injected: Vec<(Vec<String>, &Query)> = Vec::new();
    let mut leaks: Vec<String> = Vec::new();

    for q in &corpus.queries {
        let uuid = &uuid_of[&q.agent];
        let results = mem
            .recall_for_agents(&[uuid], &q.query, TOP_K, None, None, None)
            .await
            .expect("recall");

        for r in &results {
            if agent_of_key.get(r.key.as_str()) != Some(&q.agent.as_str()) {
                leaks.push(format!("{} returned {} for {}", q.id, r.key, q.agent));
            }
        }
        raw.push((results.iter().map(|r| r.key.clone()).collect(), q));

        let mut entries = results;
        apply_time_decay(&mut entries, DEFAULT_HALF_LIFE_DAYS);
        injected.push((
            entries
                .iter()
                .filter(|e| e.score.is_none_or(|s| s >= MIN_RELEVANCE))
                .map(|e| e.key.clone())
                .collect(),
            q,
        ));
    }

    assert!(leaks.is_empty(), "cross-agent leak: {leaks:?}");

    let mut current: Report = BTreeMap::new();
    let entry = current.entry(mode.to_string()).or_default();
    let raw_scores = score(&raw);
    print_table(mode, "raw", &raw_scores);
    entry.insert("raw".into(), raw_scores);
    if mode == "hybrid" {
        let inj = score(&injected);
        print_table(mode, "injected", &inj);
        entry.insert("injected".into(), inj);
    }

    let baseline_path = dir.join("baseline.json");
    let mut baseline: Report = std::fs::read(&baseline_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    if std::env::var("RECALL_BENCH_WRITE_BASELINE").is_ok() {
        baseline.insert(mode.to_string(), current[mode].clone());
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&baseline).unwrap() + "\n",
        )
        .expect("write baseline");
        eprintln!("baseline for mode {mode} written to {baseline_path:?}");
        return;
    }

    let Some(base) = baseline.get(mode) else {
        eprintln!("no baseline for mode {mode}; run with RECALL_BENCH_WRITE_BASELINE=1");
        return;
    };
    let mut regressions = Vec::new();
    for (pipeline, kinds) in &current[mode] {
        for (kind, now) in kinds {
            if let Some(was) = base.get(pipeline).and_then(|k| k.get(kind))
                && now.hit + TOLERANCE < was.hit
            {
                regressions.push(format!(
                    "{mode}/{pipeline}/{kind}: hit@{TOP_K} {:.3} -> {:.3}",
                    was.hit, now.hit
                ));
            }
        }
    }
    assert!(
        regressions.is_empty(),
        "recall regressed beyond {TOLERANCE}: {regressions:?}"
    );
}
