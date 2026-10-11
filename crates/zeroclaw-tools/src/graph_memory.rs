//! Aivory Cerveau: `graph_remember` / `graph_recall` — tools onto the
//! cognee-rs sidecar (`cerveau-server`, see
//! docs/ADR-007-CERVEAU-COGNEE-INTEGRATION.md in AVRY-V2-Main).
//!
//! Companion to `memory_store`/`memory_recall`, not a replacement: those hit
//! `zeroclaw-memory`'s pgvector hybrid recall (fast, flat entries, already
//! proven). This pair hits a knowledge graph for multi-hop relationship
//! questions a ranked list of similar chunks can't structurally answer
//! ("who is connected to X through Y").
//!
//! **Tenant-only, deliberately, and enforced at construction time.**
//! `zeroclaw-tools` cannot see `zeroclaw-runtime::agent::tenant` (the
//! dependency runs the other way — `zeroclaw-runtime` depends on
//! `zeroclaw-tools`, not back), so unlike a task-local read inside
//! `execute()`, the tenant identity is resolved once by the caller
//! (`zeroclaw-runtime`'s tool-registry construction, which *does* have
//! `current_tenant()`) and handed to `new()` as `(platform_user_id,
//! agent_type)`. The wiring caller only constructs these tools at all when
//! that pair is `Some` — see `all_tools_with_runtime` — so by the time
//! either tool's `execute()` runs, tenant identity is guaranteed present;
//! there is no runtime "no tenant" branch to fall into.
//!
//! One fixed dataset per tenant (`cerveau_graph`) rather than an
//! agent-chosen name: the sidecar already isolates by derived owner UUID
//! (proven — two tenants using the identical dataset name stay fully
//! separate), so a per-call dataset parameter would only let the model
//! fragment its own tenant's graph into silos it can't find again later,
//! for no isolation benefit.

use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::CogneeConfig;

const DATASET_NAME: &str = "cerveau_graph";

// ── Fail fast when the sidecar is down ──────────────────────────────────
//
// 2026-10-03: cerveau-server hung while staying "active" -- the kernel kept
// accepting connections into its listen backlog, nothing ever answered. With
// only the shared client's 60 s timeout, every graph_recall/graph_remember
// waited a full minute before failing, for 8 days, and pushed tool-heavy
// turns past the 180 s turn cap (AVRY-V2-Main
// docs/INCIDENT-2026-10-03-COGNEE-CERVEAU-HANG.md). Two layers now:
//
// 1. Per-request budgets for the calls a turn waits on. The client keeps its
//    60 s ceiling only for `cognify` (an LLM extraction pipeline, ~40 s when
//    run in the foreground). A healthy `add` is sub-second; a healthy
//    GraphCompletion search is 2-6 s.
// 2. A per-sidecar circuit breaker: after a transport failure (timeout,
//    refused, reset), calls to that base URL fail immediately for
//    BREAKER_COOLDOWN instead of each one waiting out its own timeout. HTTP
//    error statuses do not trip it -- the sidecar answered.

#[cfg(not(test))]
const ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(not(test))]
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
#[cfg(test)]
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
const BREAKER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

fn breaker() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static OPEN_UNTIL: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    OPEN_UNTIL.get_or_init(Default::default)
}

/// Remaining cooldown if the breaker for `base` is open.
fn breaker_open(base: &str) -> Option<std::time::Duration> {
    let map = breaker().lock().unwrap_or_else(|e| e.into_inner());
    let until = map.get(base)?;
    until.checked_duration_since(std::time::Instant::now())
}

fn trip_breaker(base: &str) {
    breaker().lock().unwrap_or_else(|e| e.into_inner()).insert(
        base.to_string(),
        std::time::Instant::now() + BREAKER_COOLDOWN,
    );
}

fn reset_breaker(base: &str) {
    breaker()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(base);
}

fn breaker_error(base: &str, remaining: std::time::Duration) -> String {
    format!(
        "graph memory unavailable: sidecar at {base} did not respond to a recent call; \
         failing fast for another {}s instead of waiting on it",
        remaining.as_secs().max(1)
    )
}

/// One shared client: building it (proxy resolution, TLS roots) is far too slow
/// for the per-turn `recall_context` path, and `reqwest::Client` is a cheap
/// `Arc` handle to share.
fn client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            zeroclaw_config::schema::build_runtime_proxy_client_with_timeouts(
                "tool.graph_memory",
                60,
                10,
            )
        })
        .clone()
}

fn apply_tenant_headers(
    builder: reqwest::RequestBuilder,
    cfg: &CogneeConfig,
    tenant_id: &str,
    agent_type: &str,
) -> reqwest::RequestBuilder {
    let mut builder = builder
        .header("X-Tenant-Id", tenant_id)
        .header("X-Agent-Type", agent_type);
    if let Some(secret) = cfg.internal_secret.as_deref().filter(|s| !s.is_empty()) {
        builder = builder.header("X-Cerveau-Internal-Secret", secret);
    }
    builder
}

/// Store durable facts in the tenant's knowledge graph -- entity and
/// relationship structure, not a flat entry. Use for facts worth answering
/// multi-hop questions about later ("X works at Y, which is part of Z");
/// use `memory_store` for everything else, it's cheaper and already proven.
pub struct GraphRememberTool {
    cfg: CogneeConfig,
    tenant_id: String,
    agent_type: String,
}

impl GraphRememberTool {
    /// `tenant_id`/`agent_type` are the raw platform values (Cerveau's
    /// `TenantContext::platform_user_id`/`agent_type`), resolved once by the
    /// caller — see the module doc for why this crate can't resolve them
    /// itself.
    pub fn new(cfg: CogneeConfig, tenant_id: String, agent_type: String) -> Self {
        Self {
            cfg,
            tenant_id,
            agent_type,
        }
    }
}

#[async_trait]
impl Tool for GraphRememberTool {
    fn name(&self) -> &str {
        "graph_remember"
    }

    fn description(&self) -> &str {
        "Store a fact in the tenant's knowledge graph for later multi-hop relationship queries \
         (e.g. 'who designed X, and where did they work before'). Extracts entities and \
         relationships automatically -- write a few sentences of real prose, not keywords. \
         Only available on a tenant turn. For a simple fact with no relationship structure to \
         extract, use memory_store instead -- it's cheaper."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The fact(s) to store, as prose. Names, relationships, and \
                                     attributes get extracted into the graph automatically."
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'text' must be non-empty".to_string()),
            });
        }

        match remember(&self.cfg, &self.tenant_id, &self.agent_type, text).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: ToolOutput::text("Stored and extracted into the knowledge graph."),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(e.to_string()),
            }),
        }
    }
}

/// Build the shared HTTP client now rather than inside the first turn's
/// recall. Cheap to call repeatedly; call once at startup when cognee is on.
pub fn warm_client() {
    let _ = client();
}

/// Send `text` to the tenant's dataset (`add`). Cheap and synchronous on the
/// sidecar; graph extraction happens separately in [`cognify`].
pub async fn add_fact(
    cfg: &CogneeConfig,
    tenant_id: &str,
    agent_type: &str,
    text: &str,
) -> anyhow::Result<()> {
    let client = client();
    let base = cfg.base_url.trim_end_matches('/');
    if let Some(remaining) = breaker_open(base) {
        anyhow::bail!(breaker_error(base, remaining));
    }

    let part = reqwest::multipart::Part::bytes(text.as_bytes().to_vec())
        .file_name("fact.txt")
        .mime_str("text/plain")
        .map_err(|e| anyhow::anyhow!("failed to build request: {e}"))?;
    let form = reqwest::multipart::Form::new()
        .part("data", part)
        .text("datasetName", DATASET_NAME);

    let add_req = apply_tenant_headers(
        client.post(format!("{base}/api/v1/add")),
        cfg,
        tenant_id,
        agent_type,
    )
    .multipart(form)
    .timeout(ADD_TIMEOUT);

    let add_resp = add_req.send().await.map_err(|e| {
        trip_breaker(base);
        anyhow::anyhow!("graph memory unavailable: {e}")
    })?;
    reset_breaker(base);
    if !add_resp.status().is_success() {
        let status = add_resp.status();
        let body = add_resp.text().await.unwrap_or_default();
        anyhow::bail!("graph_remember: add failed ({status}): {body}");
    }
    Ok(())
}

/// Run graph extraction over everything `add`ed to the tenant's dataset so
/// far. An LLM pipeline on the sidecar (measured ~40 s), so callers that
/// ingest many facts should batch it -- see `agent::graph_sync`.
pub async fn cognify(cfg: &CogneeConfig, tenant_id: &str, agent_type: &str) -> anyhow::Result<()> {
    let client = client();
    let base = cfg.base_url.trim_end_matches('/');
    if let Some(remaining) = breaker_open(base) {
        anyhow::bail!(
            "graph_remember: stored but graph extraction skipped: {}",
            breaker_error(base, remaining)
        );
    }
    let cognify_req = apply_tenant_headers(
        client.post(format!("{base}/api/v1/cognify")),
        cfg,
        tenant_id,
        agent_type,
    )
    .json(&json!({"datasets": [DATASET_NAME], "runInBackground": false}));

    let cognify_resp = cognify_req.send().await.map_err(|e| {
        trip_breaker(base);
        anyhow::anyhow!("graph_remember: stored but graph extraction request failed: {e}")
    })?;
    if !cognify_resp.status().is_success() {
        let status = cognify_resp.status();
        let body = cognify_resp.text().await.unwrap_or_default();
        anyhow::bail!("graph_remember: stored but graph extraction failed ({status}): {body}");
    }
    Ok(())
}

/// Store `text` in a tenant's knowledge graph: `add` then `cognify`. Shared
/// by `GraphRememberTool::execute` and any other caller that wants to enrich
/// the graph outside a normal tool call (e.g. `skills::review`'s
/// post-improvement hook — see that module for why skill improvements get
/// logged here, not just to disk).
pub async fn remember(
    cfg: &CogneeConfig,
    tenant_id: &str,
    agent_type: &str,
    text: &str,
) -> anyhow::Result<()> {
    add_fact(cfg, tenant_id, agent_type, text).await?;
    cognify(cfg, tenant_id, agent_type).await
}

/// Pull the human-readable texts out of a `/api/v1/search` response.
///
/// The sidecar returns `[{"searchResult": ...}]` where `searchResult` is a
/// plain string for completion-style searches and a list of
/// `{"payload": {"text": ...}}` hits for `SUMMARIES`/`CHUNKS`. Anything else is
/// ignored, so an API change degrades to "no graph context", never a failure.
pub fn extract_search_texts(body: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |text: &str| {
        let text = text.trim();
        if !text.is_empty() && !out.iter().any(|seen| seen == text) {
            out.push(text.to_string());
        }
    };
    let Some(results) = body.as_array() else {
        return out;
    };
    for result in results {
        match result.get("searchResult") {
            Some(serde_json::Value::String(text)) => push(text),
            Some(serde_json::Value::Array(hits)) => {
                for hit in hits {
                    if let Some(text) = hit
                        .get("payload")
                        .and_then(|payload| payload.get("text"))
                        .and_then(serde_json::Value::as_str)
                    {
                        push(text);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Fast graph context for injection into a turn: a `SUMMARIES` search
/// (vector lookup over the summaries extraction produced, ~1 s -- unlike
/// `GraphCompletion`, which runs an LLM and took 2-6 s), bounded by
/// `cfg.recall_timeout_ms` and `cfg.recall_max_chars`.
///
/// **Fails open by construction**: a timeout, a transport error, a non-2xx
/// (including "dataset not found" for a tenant with no graph yet) or an
/// unparseable body all return `None`. The turn must never wait on, or fail
/// because of, the graph.
pub async fn recall_context(
    cfg: &CogneeConfig,
    tenant_id: &str,
    agent_type: &str,
    query: &str,
) -> Option<String> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let base = cfg.base_url.trim_end_matches('/');
    if breaker_open(base).is_some() {
        return None;
    }
    let request = apply_tenant_headers(
        client().post(format!("{base}/api/v1/search")),
        cfg,
        tenant_id,
        agent_type,
    )
    .json(&json!({
        "query": query,
        "datasets": [DATASET_NAME],
        "searchType": "SUMMARIES",
    }))
    .send();

    // Only a transport error trips the breaker here. Running past this
    // deliberately tight per-turn budget does not: a slow-but-alive sidecar
    // should not also lose its graph_recall/graph_remember calls.
    let response = match tokio::time::timeout(
        std::time::Duration::from_millis(cfg.recall_timeout_ms.max(1)),
        request,
    )
    .await
    .ok()?
    {
        Ok(response) => {
            reset_breaker(base);
            response
        }
        Err(_) => {
            trip_breaker(base);
            return None;
        }
    };
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;

    let cap = cfg.recall_max_chars;
    let mut context = String::new();
    for text in extract_search_texts(&body) {
        let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if context.is_empty() {
            // The best hit always gets in, truncated if it alone exceeds the cap.
            context = flattened.chars().take(cap).collect();
            if flattened.chars().count() > cap {
                break;
            }
        } else if context.chars().count() + 3 + flattened.chars().count() <= cap {
            context.push_str(" | ");
            context.push_str(&flattened);
        } else {
            break;
        }
    }
    (!context.is_empty()).then_some(context)
}

/// Query the tenant's knowledge graph. Use for relationship questions
/// `memory_recall` can't structurally answer -- for a plain keyword/semantic
/// lookup, use `memory_recall` instead, it's cheaper.
pub struct GraphRecallTool {
    cfg: CogneeConfig,
    tenant_id: String,
    agent_type: String,
}

impl GraphRecallTool {
    /// See [`GraphRememberTool::new`] for why `tenant_id`/`agent_type` are
    /// resolved by the caller rather than read here.
    pub fn new(cfg: CogneeConfig, tenant_id: String, agent_type: String) -> Self {
        Self {
            cfg,
            tenant_id,
            agent_type,
        }
    }
}

#[async_trait]
impl Tool for GraphRecallTool {
    fn name(&self) -> &str {
        "graph_recall"
    }

    fn description(&self) -> &str {
        "Query the tenant's knowledge graph for relationship/multi-hop questions (e.g. 'who \
         previously worked with the engineer who designed X'). Only available on a tenant \
         turn, and only finds facts previously stored via graph_remember. For a plain \
         keyword/semantic lookup, use memory_recall instead -- it's cheaper."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A natural-language question. Phrase it as a real question, \
                                     not keywords -- the graph search reasons over relationships."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let (tenant_id, agent_type) = (self.tenant_id.as_str(), self.agent_type.as_str());

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'query' must be non-empty".to_string()),
            });
        }

        let client = client();
        let base = self.cfg.base_url.trim_end_matches('/');
        if let Some(remaining) = breaker_open(base) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(breaker_error(base, remaining)),
            });
        }

        let req = apply_tenant_headers(
            client.post(format!("{base}/api/v1/search")),
            &self.cfg,
            tenant_id,
            agent_type,
        )
        .json(&json!({"query": query, "datasets": [DATASET_NAME]}))
        .timeout(SEARCH_TIMEOUT);

        let resp = match req.send().await {
            Ok(r) => {
                reset_breaker(base);
                r
            }
            Err(e) => {
                trip_breaker(base);
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("graph memory unavailable: {e}")),
                });
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // "dataset not found" is the expected shape of "nothing stored yet" --
            // graph_remember creates the dataset lazily, so a fresh tenant with no
            // facts stored gets this, not a real error.
            let detail = body.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            if status == reqwest::StatusCode::NOT_FOUND || detail.contains("dataset not found") {
                return Ok(ToolResult {
                    success: true,
                    output: ToolOutput::text(
                        "No graph memory stored yet for this tenant -- nothing to recall.",
                    ),
                    error: None,
                });
            }
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("graph_recall failed ({status}): {body}")),
            });
        }

        let results: Vec<serde_json::Value> = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("graph_recall: could not parse response: {e}")),
                });
            }
        };

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: ToolOutput::text("No relevant graph facts found."),
                error: None,
            });
        }

        let mut text = String::new();
        for (i, r) in results.iter().enumerate() {
            let result = r
                .get("searchResult")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if i > 0 {
                text.push_str("\n\n");
            }
            text.push_str(result);
        }

        Ok(ToolResult {
            success: true,
            output: ToolOutput::text(text),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg_for(server: &MockServer) -> CogneeConfig {
        CogneeConfig {
            enabled: true,
            base_url: server.uri(),
            internal_secret: Some("test-secret".into()),
            ..CogneeConfig::default()
        }
    }

    #[test]
    fn extract_search_texts_reads_summaries_completions_and_ignores_junk() {
        let body = json!([
            {"searchResult": [
                {"payload": {"text": "Toko Melati is a prospect, budget Rp 50 juta."}},
                {"payload": {"text": "Bu Sari is the main contact."}},
                {"payload": {"text": "  Bu Sari is the main contact.  "}},
                {"payload": {"no_text": true}},
            ]},
            {"searchResult": "Kontak utama adalah Bu Sari."},
            {"searchResult": 42},
            {"unrelated": true},
        ]);
        assert_eq!(
            extract_search_texts(&body),
            vec![
                "Toko Melati is a prospect, budget Rp 50 juta.".to_string(),
                "Bu Sari is the main contact.".to_string(),
                "Kontak utama adalah Bu Sari.".to_string(),
            ],
            "trimmed, deduplicated, in order, junk ignored"
        );
        assert!(extract_search_texts(&json!({"detail": "x"})).is_empty());
        assert!(extract_search_texts(&json!(null)).is_empty());
    }

    #[tokio::test]
    async fn recall_context_returns_summaries_scoped_by_tenant_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .and(header("X-Tenant-Id", "user1"))
            .and(header("X-Agent-Type", "leads_qualifier"))
            .and(header("X-Cerveau-Internal-Secret", "test-secret"))
            .and(body_partial_json(json!({
                "searchType": "SUMMARIES",
                "datasets": ["cerveau_graph"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"searchResult": [
                    {"payload": {"text": "Toko Melati has a Rp 50 juta budget."}},
                    {"payload": {"text": "Bu Sari is the contact."}},
                ]}
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let context = recall_context(&cfg_for(&server), "user1", "leads_qualifier", "Toko Melati")
            .await
            .expect("graph context");
        assert_eq!(
            context,
            "Toko Melati has a Rp 50 juta budget. | Bu Sari is the contact."
        );
    }

    #[tokio::test]
    async fn recall_context_respects_the_character_cap() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"searchResult": [
                    {"payload": {"text": "first hit is short"}},
                    {"payload": {"text": "second hit would push the block over the cap"}},
                ]}
            ])))
            .mount(&server)
            .await;
        let cfg = CogneeConfig {
            recall_max_chars: 30,
            ..cfg_for(&server)
        };
        assert_eq!(
            recall_context(&cfg, "u", "a", "q").await.as_deref(),
            Some("first hit is short"),
            "a hit that does not fit is dropped whole"
        );

        // A single hit longer than the cap is truncated, not lost.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"searchResult": "abcdefghijklmnopqrstuvwxyz"}])),
            )
            .mount(&server)
            .await;
        let cfg = CogneeConfig {
            recall_max_chars: 10,
            ..cfg_for(&server)
        };
        assert_eq!(
            recall_context(&cfg, "u", "a", "q").await.as_deref(),
            Some("abcdefghij")
        );
    }

    #[tokio::test]
    async fn recall_context_fails_open_on_timeout_error_and_empty_graph() {
        // Slow sidecar: past the budget the turn proceeds without graph context.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(600))
                    .set_body_json(json!([{"searchResult": "late"}])),
            )
            .mount(&server)
            .await;
        let cfg = CogneeConfig {
            recall_timeout_ms: 100,
            ..cfg_for(&server)
        };
        warm_client(); // building the shared client is not what is timed
        let started = std::time::Instant::now();
        assert!(recall_context(&cfg, "u", "a", "q").await.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "must return at the timeout, not wait for the sidecar"
        );

        // Fresh tenant: the sidecar says the dataset does not exist.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"detail": "dataset not found"})),
            )
            .mount(&server)
            .await;
        assert!(
            recall_context(&cfg_for(&server), "u", "a", "q")
                .await
                .is_none()
        );

        // Sidecar down entirely.
        let cfg = CogneeConfig {
            base_url: "http://127.0.0.1:1".into(),
            ..CogneeConfig::default()
        };
        assert!(recall_context(&cfg, "u", "a", "q").await.is_none());
        // Blank query never reaches the network.
        assert!(recall_context(&cfg, "u", "a", "   ").await.is_none());
    }

    #[tokio::test]
    async fn add_fact_and_cognify_hit_the_expected_endpoints() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/add"))
            .and(header("X-Tenant-Id", "user1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/cognify"))
            .and(body_partial_json(json!({"datasets": ["cerveau_graph"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        add_fact(&cfg, "user1", "leads_qualifier", "a durable fact")
            .await
            .unwrap();
        cognify(&cfg, "user1", "leads_qualifier").await.unwrap();
    }

    /// The 2026-10-03 failure shape: a socket that is listening (connect()
    /// succeeds into the kernel backlog) but never accept()s or answers.
    fn hung_sidecar() -> (std::net::TcpListener, CogneeConfig) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = CogneeConfig {
            enabled: true,
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            ..CogneeConfig::default()
        };
        (listener, cfg)
    }

    #[tokio::test]
    async fn graph_recall_on_a_hung_sidecar_times_out_then_fails_fast() {
        let (_listener, cfg) = hung_sidecar();
        let tool = GraphRecallTool::new(cfg.clone(), "u".into(), "a".into());
        warm_client();

        let started = std::time::Instant::now();
        let first = tool
            .execute(json!({"query": "who is the contact?"}))
            .await
            .unwrap();
        assert!(!first.success);
        assert!(
            first
                .error
                .as_deref()
                .unwrap()
                .starts_with("graph memory unavailable")
        );
        assert!(
            started.elapsed() < SEARCH_TIMEOUT + std::time::Duration::from_secs(2),
            "bounded by the per-request budget, not the client's 60 s"
        );

        let started = std::time::Instant::now();
        let second = tool
            .execute(json!({"query": "who is the contact?"}))
            .await
            .unwrap();
        assert!(!second.success);
        assert!(second.error.as_deref().unwrap().contains("failing fast"));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "breaker open: no network wait at all"
        );

        // graph_remember and auto-recall share the breaker.
        let err = add_fact(&cfg, "u", "a", "fact").await.unwrap_err();
        assert!(err.to_string().contains("failing fast"));
        assert!(recall_context(&cfg, "u", "a", "q").await.is_none());
    }

    #[tokio::test]
    async fn add_fact_on_a_hung_sidecar_is_bounded_and_trips_the_breaker() {
        let (_listener, cfg) = hung_sidecar();
        warm_client();
        let started = std::time::Instant::now();
        assert!(add_fact(&cfg, "u", "a", "fact").await.is_err());
        assert!(started.elapsed() < ADD_TIMEOUT + std::time::Duration::from_secs(2));
        assert!(breaker_open(cfg.base_url.trim_end_matches('/')).is_some());
    }

    #[tokio::test]
    async fn open_breaker_skips_the_network_and_a_success_closes_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"searchResult": "ok"}])))
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let tool = GraphRecallTool::new(cfg.clone(), "u".into(), "a".into());

        trip_breaker(&server.uri());
        let skipped = tool.execute(json!({"query": "q"})).await.unwrap();
        assert!(!skipped.success);
        assert_eq!(server.received_requests().await.unwrap().len(), 0);

        // Cooldown over (simulated): the next call goes through and closes it.
        reset_breaker(&server.uri());
        let ok = tool.execute(json!({"query": "q"})).await.unwrap();
        assert!(ok.success);
        assert!(breaker_open(&server.uri()).is_none());
    }

    #[tokio::test]
    async fn http_error_status_does_not_trip_the_breaker() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({"detail": "boom"})))
            .mount(&server)
            .await;
        let tool = GraphRecallTool::new(cfg_for(&server), "u".into(), "a".into());
        let res = tool.execute(json!({"query": "q"})).await.unwrap();
        assert!(!res.success);
        assert!(
            breaker_open(&server.uri()).is_none(),
            "the sidecar answered"
        );
    }
}
