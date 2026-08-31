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

fn client() -> reqwest::Client {
    zeroclaw_config::schema::build_runtime_proxy_client_with_timeouts("tool.graph_memory", 60, 10)
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
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
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
    let client = client();
    let base = cfg.base_url.trim_end_matches('/');

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
    .multipart(form);

    let add_resp = add_req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("graph memory unavailable: {e}"))?;
    if !add_resp.status().is_success() {
        let status = add_resp.status();
        let body = add_resp.text().await.unwrap_or_default();
        anyhow::bail!("graph_remember: add failed ({status}): {body}");
    }

    let cognify_req = apply_tenant_headers(
        client.post(format!("{base}/api/v1/cognify")),
        cfg,
        tenant_id,
        agent_type,
    )
    .json(&json!({"datasets": [DATASET_NAME], "runInBackground": false}));

    let cognify_resp = cognify_req.send().await.map_err(|e| {
        anyhow::anyhow!("graph_remember: stored but graph extraction request failed: {e}")
    })?;
    if !cognify_resp.status().is_success() {
        let status = cognify_resp.status();
        let body = cognify_resp.text().await.unwrap_or_default();
        anyhow::bail!("graph_remember: stored but graph extraction failed ({status}): {body}");
    }

    Ok(())
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

        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'query' must be non-empty".to_string()),
            });
        }

        let client = client();
        let base = self.cfg.base_url.trim_end_matches('/');

        let req = apply_tenant_headers(
            client.post(format!("{base}/api/v1/search")),
            &self.cfg,
            tenant_id,
            agent_type,
        )
        .json(&json!({"query": query, "datasets": [DATASET_NAME]}));

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
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
