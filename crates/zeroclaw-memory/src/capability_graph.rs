//! Cerveau Phase 4.2: per-tenant capability graph.
//!
//! Scope note (read before extending): the planning doc's motivation for
//! this feature was "a tenant with hundreds of connected-app tool schemas
//! is too many to put in the prompt." That problem does not exist yet in
//! Cerveau — today's real per-tenant tool counts are 5-16 (see
//! `CERVEAU-STATUS.md` §2). What *is* real today: `mcp.deferred_loading`
//! collapses MCP tools into a single `tool_search` stub the model queries
//! on demand (`zeroclaw_tools::tool_search`), and that search is currently
//! pure keyword matching with no memory of which tools a tenant has
//! actually used together before. This module adds exactly that: an
//! undirected, per-tenant, learned "commonly used together" edge weight
//! between tool names, used to re-rank `tool_search`'s keyword-match
//! results. It is forward-looking in the same sense F-1 was (ADR-003,
//! patch 0025) — real infrastructure, honestly scoped to what it actually
//! does today, not a stand-in for the larger ingestion-pipeline vision in
//! the planning doc.
//!
//! Deliberately NOT built for v1 (would be premature given the above):
//! a separate `nodes` table (tool identity is just the deferred stub's
//! `prefixed_name` string — no metadata beyond that is needed yet), a
//! Composio-catalog/n8n-workflow ingestion crawler (nothing to seed from
//! that `tool_search`'s own deferred-stub set doesn't already have), and
//! embedding-based ranking (blocked on pgvector not being installed on
//! `avry-postgres` — see CERVEAU-STATUS.md §6; this uses plain weighted
//! co-occurrence instead, which needs no vector extension at all).

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use postgres::Client;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;

/// Learns and applies per-tenant "commonly used together" tool relationships.
///
/// Every method is fail-open: a backend error must never fail the calling
/// turn or tool call. `record_co_activation` and `rerank` both degrade to a
/// no-op / pass-through on error, mirroring the F-2 idempotency ledger's own
/// posture (`control_plane::tool_idem`) and the `ToolkitConnectionResolver`'s
/// (patch 0024) — a capability-graph outage must never be a turn outage.
#[async_trait]
pub trait CapabilityGraphRanker: Send + Sync {
    /// Re-rank `candidates` (deferred-tool `prefixed_name`s that already
    /// matched a `tool_search` keyword query) by learned affinity to
    /// `recent` (tool names already activated earlier this session).
    /// Returns the same set of names, reordered — never adds, removes, or
    /// deduplicates. On any backend error, returns `candidates` unchanged.
    async fn rerank(&self, tenant_id: &str, candidates: &[String], recent: &[String])
    -> Vec<String>;

    /// Record that `activated` were surfaced together by one `tool_search`
    /// call (keyword match or multi-name `select:`) — the proxy signal for
    /// "the model believes these belong to the same task." Strengthens (or
    /// creates) an undirected edge between every pair. No-op for 0 or 1
    /// names. Errors are logged and swallowed, never propagated.
    async fn record_co_activation(&self, tenant_id: &str, activated: &[String]);
}

/// Reorders `candidates` by descending learned weight (ties keep original
/// relative order — `Vec::sort_by` is a stable sort). Untracked candidates
/// (weight absent, i.e. 0.0) sort after every tracked one but otherwise keep
/// their relative order among themselves. Pure and DB-free so the ranking
/// logic itself is unit-testable without a Postgres connection.
pub fn apply_edge_boost(candidates: &[String], edge_weight: &HashMap<String, f64>) -> Vec<String> {
    let mut indexed: Vec<(usize, &String)> = candidates.iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        let wa = edge_weight.get(*a).copied().unwrap_or(0.0);
        let wb = edge_weight.get(*b).copied().unwrap_or(0.0);
        wb.partial_cmp(&wa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ia.cmp(ib))
    });
    indexed.into_iter().map(|(_, n)| n.clone()).collect()
}

/// Every unique unordered pair from `names`, each normalized so `.0 < .1`
/// (matches the edge table's `tool_a < tool_b` storage convention — halves
/// row count vs. storing both directions, and makes lookups symmetric
/// without an `OR`).
fn unique_pairs(names: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            if names[i] == names[j] {
                continue;
            }
            let (a, b) = if names[i] < names[j] {
                (names[i].clone(), names[j].clone())
            } else {
                (names[j].clone(), names[i].clone())
            };
            out.push((a, b));
        }
    }
    out
}

async fn run_on_os_thread<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("pg-capability-graph-op".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("failed to spawn pg capability graph thread")?;
    rx.await.map_err(|_| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "pg capability graph thread terminated unexpectedly"
        );
        anyhow::Error::msg("pg capability graph thread terminated unexpectedly")
    })?
}

/// Postgres-backed [`CapabilityGraphRanker`]. Own connection, independent of
/// whatever `PostgresMemory`/`PgKnowledgeGraph` instance the daemon may also
/// hold — simpler lifecycle than sharing a client, at the cost of one extra
/// long-lived connection (well inside the tuned 200-connection budget — see
/// memory `aivory-capacity-optimizations`).
pub struct PgCapabilityGraph {
    client: Arc<Mutex<Client>>,
    schema: String,
}

impl PgCapabilityGraph {
    /// Opens a new connection and ensures the schema exists. `db_url` is the
    /// same libpq key=value DSN every other Cerveau Postgres consumer uses
    /// (see CERVEAU-STATUS.md §7 — NOT a `postgresql://` URL, the prod
    /// password contains `@`/`#`).
    pub async fn connect(db_url: &str, schema: &str) -> Result<Self> {
        let db_url = db_url.to_string();
        let schema_owned = schema.to_string();
        let client = run_on_os_thread(move || {
            Client::connect(&db_url, postgres::NoTls).context("connect to Postgres")
        })
        .await?;
        let graph = Self {
            client: Arc::new(Mutex::new(client)),
            schema: schema_owned,
        };
        graph.init_schema().await?;
        Ok(graph)
    }

    async fn init_schema(&self) -> Result<()> {
        let client = Arc::clone(&self.client);
        let schema = self.schema.clone();
        run_on_os_thread(move || {
            let mut client = client.lock();
            client.batch_execute(&format!(
                r#"
                CREATE TABLE IF NOT EXISTS "{schema}".kg_capability_edges (
                    id BIGSERIAL PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    tool_a TEXT NOT NULL,
                    tool_b TEXT NOT NULL,
                    weight DOUBLE PRECISION NOT NULL DEFAULT 1.0,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    UNIQUE (tenant_id, tool_a, tool_b)
                );
                CREATE INDEX IF NOT EXISTS idx_kg_cap_edges_lookup
                    ON "{schema}".kg_capability_edges(tenant_id, tool_a);
                "#
            ))?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl CapabilityGraphRanker for PgCapabilityGraph {
    async fn rerank(
        &self,
        tenant_id: &str,
        candidates: &[String],
        recent: &[String],
    ) -> Vec<String> {
        if candidates.len() < 2 || recent.is_empty() {
            return candidates.to_vec();
        }
        let client = Arc::clone(&self.client);
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let candidates_owned = candidates.to_vec();
        let recent_owned = recent.to_vec();
        let result: Result<HashMap<String, f64>> = run_on_os_thread(move || {
            let mut client = client.lock();
            let rows = client.query(
                &format!(
                    r#"SELECT tool_a, tool_b, weight FROM "{schema}".kg_capability_edges
                       WHERE tenant_id = $1
                         AND ((tool_a = ANY($2) AND tool_b = ANY($3))
                           OR (tool_b = ANY($2) AND tool_a = ANY($3)))"#
                ),
                &[&tenant_id, &recent_owned, &candidates_owned],
            )?;
            let mut weights: HashMap<String, f64> = HashMap::new();
            for row in rows {
                let a: String = row.get(0);
                let b: String = row.get(1);
                let w: f64 = row.get(2);
                // Whichever side of the edge is the candidate (not the
                // "recent" anchor) is the one being boosted; a name can
                // appear in both sets, in which case both accumulate.
                if recent_owned.contains(&a) && candidates_owned.contains(&b) {
                    *weights.entry(b).or_insert(0.0) += w;
                } else if recent_owned.contains(&b) && candidates_owned.contains(&a) {
                    *weights.entry(a).or_insert(0.0) += w;
                }
            }
            Ok(weights)
        })
        .await;

        match result {
            Ok(weights) => apply_edge_boost(candidates, &weights),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_attrs(::serde_json::json!({ "error": e.to_string() })),
                    "capability graph rerank query failed; returning candidates unranked"
                );
                candidates.to_vec()
            }
        }
    }

    async fn record_co_activation(&self, tenant_id: &str, activated: &[String]) {
        let pairs = unique_pairs(activated);
        if pairs.is_empty() {
            return;
        }
        let client = Arc::clone(&self.client);
        let schema = self.schema.clone();
        let tenant_id = tenant_id.to_string();
        let result: Result<()> = run_on_os_thread(move || {
            let mut client = client.lock();
            let mut txn = client.transaction()?;
            for (a, b) in &pairs {
                txn.execute(
                    &format!(
                        r#"INSERT INTO "{schema}".kg_capability_edges
                               (tenant_id, tool_a, tool_b, weight, updated_at)
                           VALUES ($1, $2, $3, 1.0, NOW())
                           ON CONFLICT (tenant_id, tool_a, tool_b)
                           DO UPDATE SET weight = "{schema}".kg_capability_edges.weight + 1,
                                         updated_at = NOW()"#
                    ),
                    &[&tenant_id, a, b],
                )?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;

        if let Err(e) = result {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_attrs(::serde_json::json!({ "error": e.to_string() })),
                "capability graph co-activation write failed; usage signal dropped for this call"
            );
        }
    }
}

// ── Process-wide install hook ────────────────────────────────────────────
//
// `zeroclaw-tools` (where `ToolSearchTool` lives) already depends on
// `zeroclaw-memory` directly — unlike the tenant-id-provider hook in
// `zeroclaw-config` (which exists because THAT crate sits below
// `zeroclaw-runtime` and cannot call back up), no crate-graph inversion is
// needed here. This is a plain "install once at daemon startup, absent ⇒
// None ⇒ behavior identical to today" singleton, config-gated by
// `[capability_graph].enabled` (default `false`).
static CAPABILITY_GRAPH: OnceLock<Arc<dyn CapabilityGraphRanker>> = OnceLock::new();

/// Register the process-wide capability-graph ranker. Idempotent — only the
/// first call takes effect. Never called ⇒ [`current_capability_graph_ranker`]
/// always returns `None`, identical to pre-Phase-4.2 behavior.
pub fn install_capability_graph_ranker(ranker: Arc<dyn CapabilityGraphRanker>) {
    let _ = CAPABILITY_GRAPH.set(ranker);
}

/// The installed capability-graph ranker, if any. `None` when
/// `[capability_graph].enabled = false` (the default) or before daemon
/// startup has run its installation step (e.g. in unit tests).
pub fn current_capability_graph_ranker() -> Option<Arc<dyn CapabilityGraphRanker>> {
    CAPABILITY_GRAPH.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edge_boost_promotes_weighted_candidates_above_untracked() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut weights = HashMap::new();
        weights.insert("c".to_string(), 5.0);
        let ranked = apply_edge_boost(&candidates, &weights);
        assert_eq!(ranked, vec!["c", "a", "b"]);
    }

    #[test]
    fn apply_edge_boost_orders_multiple_weighted_candidates_by_weight_desc() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut weights = HashMap::new();
        weights.insert("a".to_string(), 1.0);
        weights.insert("b".to_string(), 3.0);
        weights.insert("c".to_string(), 2.0);
        let ranked = apply_edge_boost(&candidates, &weights);
        assert_eq!(ranked, vec!["b", "c", "a"]);
    }

    #[test]
    fn apply_edge_boost_is_stable_for_ties() {
        let candidates = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let ranked = apply_edge_boost(&candidates, &HashMap::new());
        assert_eq!(
            ranked, candidates,
            "no weights at all must leave original order untouched"
        );
    }

    #[test]
    fn apply_edge_boost_never_adds_or_drops_candidates() {
        let candidates = vec!["p".to_string(), "q".to_string()];
        let mut weights = HashMap::new();
        weights.insert("unrelated-name-not-in-candidates".to_string(), 99.0);
        let ranked = apply_edge_boost(&candidates, &weights);
        let mut sorted_in = candidates.clone();
        let mut sorted_out = ranked.clone();
        sorted_in.sort();
        sorted_out.sort();
        assert_eq!(sorted_in, sorted_out);
    }

    #[test]
    fn unique_pairs_normalizes_order_and_skips_self_pairs() {
        let names = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        let pairs = unique_pairs(&names);
        // (b,a)->(a,b), (b,b) skipped (self), (a,b) — dedup is NOT the
        // contract here (record_co_activation's UPSERT handles repeats via
        // weight accumulation); this only checks normalization + self-skip.
        for (a, b) in &pairs {
            assert!(a < b, "pair ({a}, {b}) must be lexicographically ordered");
        }
        assert!(pairs.iter().all(|(a, b)| a != b));
    }

    #[test]
    fn unique_pairs_empty_and_singleton_produce_nothing() {
        assert!(unique_pairs(&[]).is_empty());
        assert!(unique_pairs(&["solo".to_string()]).is_empty());
    }

    #[test]
    fn install_and_read_back_ranker_hook() {
        struct NoopRanker;
        #[async_trait]
        impl CapabilityGraphRanker for NoopRanker {
            async fn rerank(
                &self,
                _tenant_id: &str,
                candidates: &[String],
                _recent: &[String],
            ) -> Vec<String> {
                candidates.to_vec()
            }
            async fn record_co_activation(&self, _tenant_id: &str, _activated: &[String]) {}
        }
        // Best-effort: OnceLock is process-wide and `cargo test` runs this
        // file's tests in one process, so a prior test in this module may
        // have already installed a ranker. Either outcome (freshly
        // installed, or already-installed-from-an-earlier-test) proves the
        // hook works — assert only that *some* ranker is retrievable after
        // attempting install.
        install_capability_graph_ranker(Arc::new(NoopRanker));
        assert!(current_capability_graph_ranker().is_some());
    }
}
