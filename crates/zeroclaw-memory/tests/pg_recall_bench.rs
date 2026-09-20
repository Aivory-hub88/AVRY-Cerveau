//! Cerveau ADR-016 P0: recall-quality benchmark for the Postgres memory backend.
//!
//! Loads a synthetic, hand-written corpus (`fixtures/recall_bench/corpus.json`; English and
//! Indonesian, three agents, ages from 1 to 100 days) into a scratch schema, runs every query
//! through the real `recall_for_agents`, and reports hit@5 and MRR per query kind.
//!
//! Two pipelines are measured, because they answer different questions:
//!
//! * `raw`: what the backend returns (what the `memory_recall` tool sees).
//! * `injected`: the same results after the production auto-injection filter, i.e. the flat
//!   7-day time decay and then the `min_relevance_score = 0.4` floor (`memory_inject.rs`, live
//!   config 2026-09-20). This is what an agent sees without asking.
//! * `floor_only`: `raw` with the 0.4 floor but no decay, to separate the two causes when
//!   `injected` is low.
//!
//! Two modes, chosen by whether `fixtures/recall_bench/embeddings.json` exists:
//!
//! * `hybrid`: vectors are precomputed (same model and dimensions as production), so the run is
//!   deterministic, free and needs no network. Needs the pgvector extension.
//! * `keyword`: no embeddings, keyword-only recall. The `injected` pipeline is not reported
//!   because keyword scores are not on the hybrid scale the 0.4 floor was set for.
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
use zeroclaw_memory::importance::compute_importance;
use zeroclaw_memory::postgres::PostgresMemory;
use zeroclaw_memory::rerank::{self, RerankConfig, RerankStrategy};
use zeroclaw_memory::{Memory, MemoryCategory, MemoryEntry};

const SCHEMA: &str = "cerveau_recall_bench";
const TOP_K: usize = 5;
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
    /// Mean number of entries returned per query. For `negative` queries (nothing relevant
    /// exists) the ideal is 0, and `hit` is the share of them that returned nothing.
    #[serde(default)]
    returned: f64,
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
    // kind -> (n, hits, reciprocal ranks, entries returned)
    let mut acc: BTreeMap<String, (usize, f64, f64, f64)> = BTreeMap::new();
    for (ranked, q) in rankings {
        let (hit, rr, groups): (f64, f64, &[&str]) = if q.expect.is_empty() {
            // Nothing relevant exists: the right answer is to return nothing. Kept out of
            // "all" so it does not dilute hit@5.
            (f64::from(ranked.is_empty()), 0.0, &["negative"])
        } else {
            match ranked.iter().take(TOP_K).position(|k| q.expect.contains(k)) {
                Some(p) => (1.0, 1.0 / (p as f64 + 1.0), &[q.kind.as_str(), "all"][..]),
                None => (0.0, 0.0, &[q.kind.as_str(), "all"][..]),
            }
        };
        for key in groups {
            let e = acc.entry((*key).to_string()).or_default();
            e.0 += 1;
            e.1 += hit;
            e.2 += rr;
            e.3 += ranked.len() as f64;
        }
    }
    acc.into_iter()
        .map(|(k, (n, h, r, ret))| {
            let n_f = n as f64;
            (
                k,
                Score {
                    n,
                    hit: h / n_f,
                    mrr: r / n_f,
                    returned: ret / n_f,
                },
            )
        })
        .collect()
}

fn print_table(mode: &str, pipeline: &str, scores: &BTreeMap<String, Score>) {
    eprintln!("[{mode}/{pipeline}]");
    for (kind, s) in scores {
        eprintln!(
            "  {kind:<11} n={:<3} hit@{TOP_K}={:.3} mrr={:.3} returned={:.1}",
            s.n, s.hit, s.mrr, s.returned
        );
    }
}

/// One way of turning the backend's candidate pool into the memories an agent is shown.
/// `injected` is production today (live config 2026-09-20); the others are candidate changes.
struct Variant {
    name: &'static str,
    /// Flat 7-day time decay (the non-rerank arm of `memory_inject.rs`).
    decay: bool,
    /// The rerank stage (`memory.rerank_enabled = true`): blend, then floor. It replaces the decay.
    rerank: bool,
    /// Fill `importance` with the heuristic scorer, as ADR-016 P1 would on store.
    importance: bool,
    floor: f64,
}

const VARIANTS: &[Variant] = &[
    Variant {
        name: "rerank_f0.2",
        decay: false,
        rerank: true,
        importance: false,
        floor: 0.2,
    },
    Variant {
        name: "injected",
        decay: true,
        rerank: false,
        importance: false,
        floor: 0.4,
    },
    Variant {
        name: "floor_only",
        decay: false,
        rerank: false,
        importance: false,
        floor: 0.4,
    },
    Variant {
        name: "decay_f0.3",
        decay: true,
        rerank: false,
        importance: false,
        floor: 0.3,
    },
    Variant {
        name: "decay_f0.2",
        decay: true,
        rerank: false,
        importance: false,
        floor: 0.2,
    },
    Variant {
        name: "nodecay_f0.3",
        decay: false,
        rerank: false,
        importance: false,
        floor: 0.3,
    },
    Variant {
        name: "nodecay_f0.2",
        decay: false,
        rerank: false,
        importance: false,
        floor: 0.2,
    },
    Variant {
        name: "rerank_f0.4",
        decay: false,
        rerank: true,
        importance: false,
        floor: 0.4,
    },
    Variant {
        name: "rerank_f0.3",
        decay: false,
        rerank: true,
        importance: false,
        floor: 0.3,
    },
    Variant {
        name: "rerank_imp_f0.4",
        decay: false,
        rerank: true,
        importance: true,
        floor: 0.4,
    },
    Variant {
        name: "rerank_imp_f0.3",
        decay: false,
        rerank: true,
        importance: true,
        floor: 0.3,
    },
];

/// Pipelines whose hit@5 is enforced against the baseline. The other variants are
/// informational: they are candidates, not behaviour we ship.
const ENFORCED: &[&str] = &["raw", "injected"];

/// Candidate pool size: `limit * candidate_multiplier` (default 4), as production over-fetches
/// only when rerank is on. Non-rerank variants use the first `TOP_K` of it, which is exactly
/// what a `TOP_K` recall returns because the backend orders by score.
const POOL: usize = TOP_K * 4;

fn apply(v: &Variant, pool: &[MemoryEntry]) -> Vec<MemoryEntry> {
    let mut entries = pool.to_vec();
    if v.rerank {
        if v.importance {
            for e in &mut entries {
                e.importance = Some(compute_importance(&e.content, &e.category));
            }
        }
        let cfg = RerankConfig {
            strategy: RerankStrategy::None,
            threshold: 5,
            importance_weight: 0.2,
            recency_weight: 0.1,
            min_relevance_score: v.floor,
            final_limit: TOP_K,
            candidate_pool_cap: POOL,
        };
        return rerank::run(entries, &cfg, |e| {
            !matches!(e.category, MemoryCategory::Conversation)
        });
    }
    entries.truncate(TOP_K);
    if v.decay {
        apply_time_decay(&mut entries, DEFAULT_HALF_LIFE_DAYS);
    }
    entries.retain(|e| e.score.is_none_or(|s| s >= v.floor));
    entries
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
    let mut variant_out: Vec<Vec<(Vec<String>, &Query)>> =
        VARIANTS.iter().map(|_| Vec::new()).collect();
    // (age of the expected memory, its raw score) for every query whose answer was retrieved.
    let mut expected_scores: Vec<(i64, f64)> = Vec::new();
    let age_of: HashMap<&str, i64> = corpus
        .memories
        .iter()
        .map(|m| (m.id.as_str(), m.age_days))
        .collect();
    let mut leaks: Vec<String> = Vec::new();

    for q in &corpus.queries {
        let uuid = &uuid_of[&q.agent];
        let pool = mem
            .recall_for_agents(&[uuid], &q.query, POOL, None, None, None)
            .await
            .expect("recall");
        let top: Vec<MemoryEntry> = pool.iter().take(TOP_K).cloned().collect();

        for r in &pool {
            if agent_of_key.get(r.key.as_str()) != Some(&q.agent.as_str()) {
                leaks.push(format!("{} returned {} for {}", q.id, r.key, q.agent));
            }
        }
        raw.push((top.iter().map(|r| r.key.clone()).collect(), q));
        if let Some(hit) = top.iter().find(|r| q.expect.contains(&r.key)) {
            expected_scores.push((age_of[hit.key.as_str()], hit.score.unwrap_or(0.0)));
        }
        if mode == "hybrid" {
            for (out, v) in variant_out.iter_mut().zip(VARIANTS) {
                out.push((apply(v, &pool).iter().map(|e| e.key.clone()).collect(), q));
            }
        }
    }

    assert!(leaks.is_empty(), "cross-agent leak: {leaks:?}");

    let mut current: Report = BTreeMap::new();
    let entry = current.entry(mode.to_string()).or_default();
    let raw_scores = score(&raw);
    print_table(mode, "raw", &raw_scores);
    entry.insert("raw".into(), raw_scores);
    if mode == "hybrid" {
        for (out, v) in variant_out.iter().zip(VARIANTS) {
            let sc = score(out);
            if v.name == "injected" {
                print_table(mode, v.name, &sc);
            }
            entry.insert(v.name.to_string(), sc);
        }
        for (label, lo, hi) in [("<=7d", 0, 7), ("8-30d", 8, 30), (">30d", 31, i64::MAX)] {
            let v: Vec<f64> = expected_scores
                .iter()
                .filter(|(a, _)| (lo..=hi).contains(a))
                .map(|(_, sc)| *sc)
                .collect();
            if !v.is_empty() {
                eprintln!(
                    "  raw score of the expected hit, age {label:<6} n={:<2} mean={:.3} max={:.3}",
                    v.len(),
                    v.iter().sum::<f64>() / v.len() as f64,
                    v.iter().cloned().fold(0.0, f64::max)
                );
            }
        }
        eprintln!(
            "\nVariant comparison (hit@{TOP_K}; `neg` = share of no-answer queries that correctly return nothing; `ret` = mean entries shown):"
        );
        eprintln!(
            "  {:<16} {:>5} {:>10} {:>10} {:>5} {:>5} {:>5}",
            "variant", "all", "paraphrase", "indonesian", "old", "neg", "ret"
        );
        for name in std::iter::once("raw").chain(VARIANTS.iter().map(|v| v.name)) {
            let k = &entry[name];
            let g = |kind: &str| k.get(kind).map_or(f64::NAN, |s| s.hit);
            eprintln!(
                "  {name:<16} {:>5.3} {:>10.3} {:>10.3} {:>5.3} {:>5.2} {:>5.1}",
                g("all"),
                g("paraphrase"),
                g("indonesian"),
                g("old"),
                g("negative"),
                k["all"].returned
            );
        }
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
    for (pipeline, kinds) in current[mode]
        .iter()
        .filter(|(p, _)| ENFORCED.contains(&p.as_str()))
    {
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
