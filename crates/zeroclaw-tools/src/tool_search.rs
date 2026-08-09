//! Built-in `tool_search` tool for on-demand MCP tool schema loading.
//!
//! When `mcp.deferred_loading` is enabled, this tool lets the LLM discover and
//! activate deferred MCP tools. Supports two query modes:
//! - `select:name1,name2` — fetch exact tools by prefixed name.
//! - Free-text keyword search — returns the best-matching stubs.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::mcp_deferred::{ActivatedToolSet, DeferredMcpToolSet};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

/// Default maximum number of search results.
const DEFAULT_MAX_RESULTS: usize = 5;

type ActivationHook = Arc<dyn Fn(Arc<dyn Tool>) + Send + Sync>;

/// Tool-level access policy applied at discovery time.
///
/// When set on `ToolSearchTool`, deferred tools that fail this check are
/// never surfaced to the LLM and never activated — keeping them out of
/// the context window entirely.
///
/// The policy carries two independent allow-list gates that are AND-ed
/// together, plus a single deny-list:
///
/// - `allowed`: the agent's risk-profile allow-list. The MCP
///   `<server>__<tool>` auto-admit exception (any name containing `__`
///   passes when the list is non-empty) applies **only** to this gate.
///   This is the high-risk default-accept-unless-denied shift introduced
///   in PR #7547 so that the post-#7464 `mcp.enabled = true` default
///   actually surfaces discovered MCP tools to agents.
/// - `caller_allowed`: a caller-supplied per-run allow-list (cron job
///   `allowed_tools`, narrowed delegate invocations, etc.). This is a
///   strict explicit-list intersection — there is **no** MCP auto-admit
///   on this gate. PR #7547 review (Audacity88, singlerider) called out
///   that collapsing this list into `allowed` made per-run narrowing
///   stop working as a capability boundary the moment an MCP server was
///   configured.
/// - `denied`: subtracts from the final set. Applies to both gates and
///   to auto-admitted MCP names.
#[derive(Clone, Default)]
pub struct ToolAccessPolicy {
    pub allowed: Option<Vec<String>>,
    pub caller_allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
}

impl ToolAccessPolicy {
    /// Construct from a `SecurityPolicy`'s tool fields and an optional
    /// caller-supplied allowlist. Used by both `run()` and
    /// `process_message()` to keep policy construction in sync.
    ///
    /// The risk-profile `allowed_tools` and the caller-supplied
    /// `caller_allowed` are kept as two separate gates inside the
    /// returned policy. Per PR #7547 review, this is required so the
    /// MCP `<server>__<tool>` auto-admit exception that applies to the
    /// risk-profile gate does **not** silently widen narrower per-run
    /// allow-lists.
    pub fn from_security(
        allowed_tools: Option<&[String]>,
        excluded_tools: Option<&[String]>,
        caller_allowed: Option<&[String]>,
    ) -> Option<Self> {
        let mut policy = Self::default();
        if let Some(list) = allowed_tools {
            policy.allowed = Some(list.to_vec());
        }
        if let Some(caller) = caller_allowed {
            policy.caller_allowed = Some(caller.to_vec());
        }
        if let Some(list) = excluded_tools {
            policy.denied = Some(list.to_vec());
        }
        if policy.allowed.is_some() || policy.caller_allowed.is_some() || policy.denied.is_some() {
            Some(policy)
        } else {
            None
        }
    }

    pub fn is_tool_allowed(&self, name: &str) -> bool {
        // Deny-list always wins.
        let in_deny = self
            .denied
            .as_ref()
            .is_some_and(|list| list.iter().any(|t| t == name));
        if in_deny {
            return false;
        }

        // Risk-profile gate: MCP `<server>__<tool>` names are auto-admitted
        // when the list is non-empty. An explicit empty list (`Some(vec![])`)
        // still means "deny everything".
        let risk_ok = match self.allowed.as_ref() {
            None => true,
            Some(list) if list.is_empty() => false,
            Some(list) => list.iter().any(|t| t == name) || name.contains("__"),
        };
        if !risk_ok {
            return false;
        }

        // Caller-supplied per-run gate: strict explicit-list intersection.
        // No MCP auto-admit here — per PR #7547 review, that exception is
        // scoped to the risk-profile gate so per-run narrowing (cron jobs,
        // narrowed delegate invocations) remains a reliable capability
        // boundary even when an MCP server is configured.
        match self.caller_allowed.as_ref() {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        }
    }
}

/// Cerveau Phase 4.2: per-tenant identity + ranker for capability-graph
/// reranking of keyword-search results. `None` (the default) leaves
/// `tool_search` bit-for-bit unchanged from before this feature existed.
#[cfg(feature = "memory-postgres")]
type CapabilityGraphHandle = (
    String,
    Arc<dyn zeroclaw_memory::capability_graph::CapabilityGraphRanker>,
);

/// Built-in tool that fetches full schemas for deferred MCP tools.
pub struct ToolSearchTool {
    deferred: DeferredMcpToolSet,
    activated: Arc<Mutex<ActivatedToolSet>>,
    access_policy: Option<ToolAccessPolicy>,
    activation_hook: Option<ActivationHook>,
    #[cfg(feature = "memory-postgres")]
    capability_graph: Option<CapabilityGraphHandle>,
}

impl ToolSearchTool {
    pub fn new(deferred: DeferredMcpToolSet, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred,
            activated,
            access_policy: None,
            activation_hook: None,
            #[cfg(feature = "memory-postgres")]
            capability_graph: None,
        }
    }

    pub fn with_access_policy(mut self, policy: ToolAccessPolicy) -> Self {
        self.access_policy = Some(policy);
        self
    }

    /// Attach a per-tenant capability-graph ranker. `tenant_id` should be
    /// `TenantContext::tenant_id` (already `[A-Za-z0-9._-]+`-validated by
    /// the gateway); the ranker is typically
    /// `zeroclaw_memory::capability_graph::current_capability_graph_ranker()`,
    /// which is `None` unless `[capability_graph].enabled = true` and a
    /// Postgres memory backend is configured — in which case this method
    /// is simply never called and behavior is unchanged.
    #[cfg(feature = "memory-postgres")]
    pub fn with_capability_graph(
        mut self,
        tenant_id: String,
        ranker: Arc<dyn zeroclaw_memory::capability_graph::CapabilityGraphRanker>,
    ) -> Self {
        self.capability_graph = Some((tenant_id, ranker));
        self
    }

    pub fn with_activation_hook(mut self, hook: ActivationHook) -> Self {
        self.activation_hook = Some(hook);
        self
    }

    fn is_allowed(&self, tool_name: &str) -> bool {
        self.access_policy
            .as_ref()
            .is_none_or(|p| p.is_tool_allowed(tool_name))
    }

    fn notify_activated(&self, tools: Vec<Arc<dyn Tool>>) {
        if let Some(hook) = &self.activation_hook {
            for tool in tools {
                hook(tool);
            }
        }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Fetch full schema definitions for deferred MCP tools so they can be called. \
         Use \"select:name1,name2\" for exact match or keywords to search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "description": "Query to find deferred tools. Use \"select:<tool_name>\" for direct selection, or keywords to search.",
                    "type": "string"
                },
                "max_results": {
                    "description": "Maximum number of results to return (default: 5)",
                    "type": "number",
                    "default": DEFAULT_MAX_RESULTS
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(DEFAULT_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);

        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("query parameter is required".into()),
            });
        }

        // Parse query mode
        if let Some(names_str) = query.strip_prefix("select:") {
            // Exact selection mode
            let names: Vec<&str> = names_str.split(',').map(str::trim).collect();
            return self.select_tools(&names).await;
        }

        // Keyword search mode.
        // When a policy is active, fetch all matches so denied tools don't
        // consume result slots. Same reasoning extends to Phase 4.2's
        // capability-graph rerank: it can only promote a lower-keyword-rank
        // candidate above max_results's cutoff if it's actually in the pool
        // reranking sees — truncating to max_results before reranking would
        // make the rerank step unable to change anything it doesn't already
        // agree with. The max_results cap is applied after filtering/reranking.
        #[cfg(feature = "memory-postgres")]
        let widen_for_rerank = self.capability_graph.is_some();
        #[cfg(not(feature = "memory-postgres"))]
        let widen_for_rerank = false;
        let search_limit = if self.access_policy.is_some() || widen_for_rerank {
            usize::MAX
        } else {
            max_results
        };
        let results = self.deferred.search(query, search_limit);
        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching deferred tools found.".into(),
                error: None,
            });
        }

        // Cerveau (Phase 4.2): rerank by learned per-tenant "commonly used
        // together" affinity to whatever's already activated this session.
        // A no-op (order unchanged) when no ranker is attached, or when
        // nothing is activated yet to rank against — see
        // `zeroclaw_memory::capability_graph`'s module docs for scope.
        #[cfg(feature = "memory-postgres")]
        let results: Vec<_> = match &self.capability_graph {
            Some((tenant_id, ranker)) => {
                let recent: Vec<String> = {
                    let guard = self
                        .activated
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.tool_specs().iter().map(|s| s.name.clone()).collect()
                };
                if recent.is_empty() {
                    results
                } else {
                    let candidates: Vec<String> =
                        results.iter().map(|s| s.prefixed_name.clone()).collect();
                    let ranked_names = ranker.rerank(tenant_id, &candidates, &recent).await;
                    let by_name: std::collections::HashMap<&str, _> =
                        results.iter().map(|s| (s.prefixed_name.as_str(), *s)).collect();
                    ranked_names
                        .iter()
                        .filter_map(|n| by_name.get(n.as_str()).copied())
                        .collect()
                }
            }
            None => results,
        };

        // Activate and return full specs (policy-filtered, then capped)
        let mut output = String::from("<functions>\n");
        let mut activated_count = 0;
        // Tools surfaced together in this call — the capability-graph
        // "used together" proxy signal (Phase 4.2). Collected unconditionally
        // (cheap: `String` pushes) so the lock-scope block below stays
        // identical regardless of the `memory-postgres` feature; only its
        // *consumer* after the block is feature-gated.
        let mut surfaced_names: Vec<String> = Vec::new();
        // Explicit block, not a bare `drop(guard)`: guarantees the
        // non-`Send` `MutexGuard` is lexically out of scope before the
        // `record_co_activation(...).await` below, regardless of how the
        // `#[async_trait]`-generated future's drop-tracking treats an
        // explicit `drop()` call on a match-bound bindings.
        let newly_activated = {
            let mut newly_activated = Vec::new();
            let mut returned_count = 0;
            let mut guard = match self.activated.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "query": query,
                                "mode": "keyword_search",
                            })),
                        "tool_search activated-tool lock poisoned during keyword activation; recovering guard"
                    );
                    poisoned.into_inner()
                }
            };

            for stub in &results {
                if returned_count >= max_results {
                    break;
                }
                if !self.is_allowed(&stub.prefixed_name) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!(
                            "tool_search: '{}' matched query but denied by access policy",
                            stub.prefixed_name
                        )
                    );
                    continue;
                }
                if let Some(spec) = self.deferred.tool_spec(&stub.prefixed_name) {
                    if !guard.is_activated(&stub.prefixed_name)
                        && let Some(tool) = self.deferred.activate(&stub.prefixed_name)
                    {
                        let tool: Arc<dyn Tool> = Arc::from(tool);
                        guard.activate(stub.prefixed_name.clone(), Arc::clone(&tool));
                        newly_activated.push(tool);
                        activated_count += 1;
                    }
                    let _ = writeln!(
                        output,
                        "<function>{{\"name\": \"{}\", \"description\": \"{}\", \"parameters\": {}}}</function>",
                        spec.name,
                        spec.description.replace('"', "\\\""),
                        spec.parameters
                    );
                    surfaced_names.push(stub.prefixed_name.clone());
                    returned_count += 1;
                }
            }

            newly_activated
        };

        output.push_str("</functions>\n");
        self.notify_activated(newly_activated);

        // Cerveau (Phase 4.2): the tools surfaced together by one keyword
        // search are the "used together" proxy signal — strengthens (or
        // creates) a learned edge between every pair. Fire-and-forget: a
        // backend error here is logged and swallowed inside
        // `record_co_activation` itself, never propagated to this call.
        #[cfg(feature = "memory-postgres")]
        if let Some((tenant_id, ranker)) = &self.capability_graph {
            ranker.record_co_activation(tenant_id, &surfaced_names).await;
        }

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "tool_search: query={query:?}, matched={}, activated={activated_count}",
                results.len()
            )
        );

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

impl ToolSearchTool {
    async fn select_tools(&self, names: &[&str]) -> anyhow::Result<ToolResult> {
        let mut output = String::from("<functions>\n");
        let mut not_found = Vec::new();
        let mut activated_count = 0;
        // Same rationale as `execute`'s keyword-search branch: collected
        // unconditionally, consumed only behind the feature gate below.
        let mut found_names: Vec<String> = Vec::new();
        // Explicit block (not a bare `drop(guard)`) so the non-`Send`
        // `MutexGuard` is lexically out of scope before the
        // `record_co_activation(...).await` below.
        let newly_activated = {
            let mut newly_activated = Vec::new();
            let mut guard = match self.activated.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "requested_names": names,
                                "mode": "select",
                            })),
                        "tool_search activated-tool lock poisoned during select activation; recovering guard"
                    );
                    poisoned.into_inner()
                }
            };

            for name in names {
                if name.is_empty() {
                    continue;
                }
                if !self.is_allowed(name) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!("tool_search select: '{}' denied by access policy", name)
                    );
                    not_found.push(*name);
                    continue;
                }
                match self.deferred.tool_spec(name) {
                    Some(spec) => {
                        if !guard.is_activated(name)
                            && let Some(tool) = self.deferred.activate(name)
                        {
                            let tool: Arc<dyn Tool> = Arc::from(tool);
                            guard.activate(String::from(*name), Arc::clone(&tool));
                            newly_activated.push(tool);
                            activated_count += 1;
                        }
                        let _ = writeln!(
                            output,
                            "<function>{{\"name\": \"{}\", \"description\": \"{}\", \"parameters\": {}}}</function>",
                            spec.name,
                            spec.description.replace('"', "\\\""),
                            spec.parameters
                        );
                        found_names.push((*name).to_string());
                    }
                    None => {
                        not_found.push(*name);
                    }
                }
            }

            newly_activated
        };

        output.push_str("</functions>\n");
        self.notify_activated(newly_activated);

        // Cerveau (Phase 4.2): a multi-name `select:` call is an even
        // stronger "these belong together" signal than a keyword search —
        // the caller named them explicitly. Same fail-open contract as the
        // keyword-search path above.
        #[cfg(feature = "memory-postgres")]
        if let Some((tenant_id, ranker)) = &self.capability_graph {
            ranker.record_co_activation(tenant_id, &found_names).await;
        }

        if !not_found.is_empty() {
            let _ = write!(output, "\nNot found: {}", not_found.join(", "));
        }

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "tool_search select: requested={}, activated={activated_count}, not_found={}",
                names.len(),
                not_found.len()
            )
        );

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpRegistry;
    use crate::mcp_deferred::DeferredMcpToolStub;
    use crate::mcp_protocol::McpToolDef;

    async fn make_deferred_set(stubs: Vec<DeferredMcpToolStub>) -> DeferredMcpToolSet {
        let registry = Arc::new(McpRegistry::connect_all(&[]).await.unwrap());
        DeferredMcpToolSet { stubs, registry }
    }

    fn make_stub(name: &str, desc: &str) -> DeferredMcpToolStub {
        let def = McpToolDef {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        DeferredMcpToolStub::new(name.to_string(), def)
    }

    fn assert_poisoned_activated_contains(
        activated: &Arc<Mutex<ActivatedToolSet>>,
        tool_name: &str,
    ) {
        let guard = activated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(guard.is_activated(tool_name));
    }

    #[tokio::test]
    async fn tool_metadata() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        assert_eq!(tool.name(), "tool_search");
        assert!(!tool.description().is_empty());
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": ""}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn select_nonexistent_tool_reports_not_found() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "select:nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Not found"));
    }

    #[tokio::test]
    async fn keyword_search_no_matches() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read file")]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "zzzzz_nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No matching"));
    }

    #[tokio::test]
    async fn keyword_search_finds_match() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );
        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        // Tool should now be activated
        assert!(activated.lock().unwrap().is_activated("fs__read"));
    }

    #[tokio::test]
    async fn keyword_search_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );

        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        assert_poisoned_activated_contains(&activated, "fs__read");
    }

    /// Verify tool_search works with stubs from multiple MCP servers,
    /// simulating a daemon-mode setup where several servers are deferred.
    #[tokio::test]
    async fn multiple_servers_stubs_all_searchable() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("server_a__list_files", "List files on server A"),
            make_stub("server_a__read_file", "Read file on server A"),
            make_stub("server_b__query_db", "Query database on server B"),
            make_stub("server_b__insert_row", "Insert row on server B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Search should find tools across both servers
        let result = tool
            .execute(serde_json::json!({"query": "file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_a__list_files"));
        assert!(result.output.contains("server_a__read_file"));

        // Server B tools should also be searchable
        let result = tool
            .execute(serde_json::json!({"query": "database query"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_b__query_db"));
    }

    /// Verify select mode activates tools and they stay activated across calls,
    /// matching the daemon-mode pattern where a single ActivatedToolSet persists.
    #[tokio::test]
    async fn select_activates_and_persists_across_calls() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Activate tool_a
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(activated.lock().unwrap().is_activated("srv__tool_a"));
        assert!(!activated.lock().unwrap().is_activated("srv__tool_b"));

        // Activate tool_b in a separate call
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_b"}))
            .await
            .unwrap();
        assert!(result.success);

        // Both should remain activated
        let guard = activated.lock().unwrap();
        assert!(guard.is_activated("srv__tool_a"));
        assert!(guard.is_activated("srv__tool_b"));
        assert_eq!(guard.tool_specs().len(), 2);
    }

    #[tokio::test]
    async fn select_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("srv__tool_a"));
        assert_poisoned_activated_contains(&activated, "srv__tool_a");
    }

    /// Verify re-activating an already-activated tool does not duplicate it.
    #[tokio::test]
    async fn reactivation_is_idempotent() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        );

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(activated.lock().unwrap().tool_specs().len(), 1);
    }

    #[tokio::test]
    async fn activation_hook_receives_newly_activated_tools_once() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            seen_hook.lock().unwrap().push(tool.name().to_string());
        }));

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(seen.lock().unwrap().as_slice(), ["srv__tool"]);
    }

    #[test]
    fn policy_none_is_unrestricted() {
        let p = ToolAccessPolicy::default();
        assert!(p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("anything"));
    }

    #[test]
    fn policy_allowlist_admits_only_listed() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(p.is_tool_allowed("shell"));
        assert!(!p.is_tool_allowed("file_write"));
    }

    #[test]
    fn policy_denylist_rejects_listed() {
        let p = ToolAccessPolicy {
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[tokio::test]
    async fn policy_filters_keyword_search_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__allowed_tool", "An allowed tool"),
            make_stub("srv__blocked_tool", "A blocked tool"),
        ];
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__blocked_tool".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__blocked_tool"));
        assert!(!activated.lock().unwrap().is_activated("srv__blocked_tool"));
    }

    #[tokio::test]
    async fn policy_denied_tool_does_not_consume_max_results_slot() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        // "denied_tool" ranks higher (more keyword matches) but is blocked.
        // "allowed_tool" ranks lower but should still be returned with max_results=1.
        let stubs = vec![
            make_stub("srv__denied_tool", "tool for searching files"),
            make_stub("srv__allowed_tool", "tool for files"),
        ];
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__denied_tool".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "searching files", "max_results": 1}))
            .await
            .unwrap();
        assert!(result.success);
        // The allowed tool should be returned even though max_results=1
        // and the denied tool ranked higher.
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__denied_tool"));
        assert!(activated.lock().unwrap().is_activated("srv__allowed_tool"));
    }

    #[tokio::test]
    async fn policy_filters_select_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__ok", "OK tool"),
            make_stub("srv__nope", "Blocked tool"),
        ];
        // Runtime-discovered MCP tools (names containing "__") are auto-admitted
        // when an allow-list is present, so the operator-visible way to block a
        // specific MCP tool is the deny-list (the `excluded_tools` equivalent).
        // See `ToolAccessPolicy::is_tool_allowed` and PR #7547.
        let policy = ToolAccessPolicy {
            allowed: Some(vec!["srv__ok".into()]),
            denied: Some(vec!["srv__nope".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__ok,srv__nope"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__ok"));
        assert!(!result.output.contains("\"name\": \"srv__nope\""));
        assert!(result.output.contains("Not found"));
        assert!(!activated.lock().unwrap().is_activated("srv__nope"));
    }

    /// PR #7547 review (Audacity88 / singlerider) — second-round blocking:
    /// the MCP `<server>__<tool>` auto-admit exception must apply ONLY to
    /// the risk-profile allow-list, not to the caller-supplied per-run
    /// `allowed_tools`. Otherwise a cron job that narrows
    /// `allowed_tools = ["cron_add"]` would still surface every
    /// runtime-discovered MCP wrapper, breaking per-job capability
    /// narrowing the moment an MCP server is configured.
    ///
    /// This test fixes `from_security` semantics so an MCP name the
    /// caller did not explicitly include is rejected even when the
    /// risk-profile allow-list would auto-admit it.
    #[test]
    fn caller_allowed_per_run_gate_does_not_auto_admit_mcp_names() {
        // The risk-profile gate is wide (unrestricted), so the MCP
        // auto-admit would happily pass any `__` name. The caller-supplied
        // per-run list narrows down to a single non-MCP tool (`cron_add`).
        let policy = ToolAccessPolicy::from_security(None, None, Some(&["cron_add".to_string()]))
            .expect("caller-supplied list should produce a policy");

        assert!(
            policy.is_tool_allowed("cron_add"),
            "cron_add must pass — it is in the caller list"
        );
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper outside the caller list must be rejected, but \
             was admitted — the per-run gate is leaking the risk-profile \
             MCP auto-admit exception (PR #7547 review regression)"
        );
        assert!(
            !policy.is_tool_allowed("github__search"),
            "second MCP wrapper outside the caller list must also be \
             rejected (PR #7547 review regression)"
        );
    }

    /// Companion to the test above: even when the risk profile DOES have
    /// a non-empty allow-list (so the auto-admit branch is live on that
    /// gate), the caller-supplied per-run list still narrows the final
    /// set strictly. The risk-profile auto-admit must not leak past the
    /// per-run gate.
    #[test]
    fn caller_allowed_per_run_gate_narrows_after_risk_profile_auto_admit() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["shell".to_string(), "github__search".to_string()]),
        )
        .expect("risk + caller lists should produce a policy");

        // `shell`: in risk allow + in caller list → admitted.
        assert!(policy.is_tool_allowed("shell"));
        // `github__search`: auto-admitted by risk MCP exception + in caller
        // list → admitted.
        assert!(policy.is_tool_allowed("github__search"));
        // `filesystem__write_file`: auto-admitted by risk MCP exception
        // (would pass the risk gate) but NOT in caller list → rejected.
        // This is the per-run narrowing the bug used to break.
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper not in caller list must be rejected even when \
             the risk-profile auto-admit would let it through"
        );
        // Non-MCP outside both lists: rejected.
        assert!(!policy.is_tool_allowed("memory_recall"));
    }

    /// `excluded_tools` must subtract regardless of which gate admitted
    /// the name. Pins the deny-list contract across the refactor.
    #[test]
    fn caller_allowed_per_run_gate_still_honors_denylist() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            Some(&["filesystem__write_file".to_string()]),
            Some(&["shell".to_string(), "filesystem__write_file".to_string()]),
        )
        .expect("policy with all three fields should be constructed");

        assert!(policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "denylist subtracts even when both gates would admit"
        );
    }

    // ── Cerveau Phase 4.2: capability-graph wiring ──────────────────────

    #[cfg(feature = "memory-postgres")]
    mod capability_graph_wiring {
        use super::*;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;
        use zeroclaw_memory::capability_graph::CapabilityGraphRanker;

        /// Records every call it receives and, if configured, reverses
        /// candidate order — enough to prove `ToolSearchTool` actually
        /// threads its ranker through, without needing a real Postgres.
        #[derive(Default)]
        struct FakeRanker {
            reverse: bool,
            rerank_calls: StdMutex<Vec<(String, Vec<String>, Vec<String>)>>,
            co_activation_calls: StdMutex<Vec<(String, Vec<String>)>>,
        }

        #[async_trait]
        impl CapabilityGraphRanker for FakeRanker {
            async fn rerank(
                &self,
                tenant_id: &str,
                candidates: &[String],
                recent: &[String],
            ) -> Vec<String> {
                self.rerank_calls.lock().unwrap().push((
                    tenant_id.to_string(),
                    candidates.to_vec(),
                    recent.to_vec(),
                ));
                if self.reverse {
                    let mut v = candidates.to_vec();
                    v.reverse();
                    v
                } else {
                    candidates.to_vec()
                }
            }

            async fn record_co_activation(&self, tenant_id: &str, activated: &[String]) {
                self.co_activation_calls
                    .lock()
                    .unwrap()
                    .push((tenant_id.to_string(), activated.to_vec()));
            }
        }

        #[tokio::test]
        async fn without_recent_activity_rerank_is_never_called() {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let ranker = Arc::new(FakeRanker {
                reverse: true,
                ..Default::default()
            });
            let tool = ToolSearchTool::new(
                make_deferred_set(vec![
                    make_stub("srv__a", "tool for files"),
                    make_stub("srv__b", "tool for files"),
                ])
                .await,
                Arc::clone(&activated),
            )
            .with_capability_graph("tenant-x".to_string(), Arc::clone(&ranker) as Arc<_>);

            // Nothing activated yet this session ⇒ `recent` is empty ⇒
            // `execute`'s own short-circuit must skip calling `rerank` at
            // all (matches `PgCapabilityGraph::rerank`'s own `recent.is_empty()`
            // fast path — the *caller* short-circuits identically).
            let result = tool
                .execute(serde_json::json!({"query": "files"}))
                .await
                .unwrap();
            assert!(result.success);
            assert!(result.output.contains("srv__a"));
            assert!(result.output.contains("srv__b"));
            assert!(
                ranker.rerank_calls.lock().unwrap().is_empty(),
                "rerank must not be called when nothing is activated yet to rank against"
            );
        }

        #[tokio::test]
        async fn reranks_keyword_search_results_using_the_attached_ranker() {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            // Pre-activate a tool so `recent` is non-empty and the rerank
            // path actually engages.
            let seed_tool: Arc<dyn Tool> = Arc::new(
                make_stub("srv__seed", "seed tool").activate(Arc::new(
                    McpRegistry::connect_all(&[]).await.unwrap(),
                )),
            );
            activated
                .lock()
                .unwrap()
                .activate("srv__seed".to_string(), seed_tool);

            let ranker = Arc::new(FakeRanker {
                reverse: true,
                ..Default::default()
            });
            let tool = ToolSearchTool::new(
                make_deferred_set(vec![
                    make_stub("srv__a", "tool for files"),
                    make_stub("srv__b", "tool for files"),
                ])
                .await,
                Arc::clone(&activated),
            )
            .with_capability_graph("tenant-x".to_string(), Arc::clone(&ranker) as Arc<_>);

            let result = tool
                .execute(serde_json::json!({"query": "files", "max_results": 1}))
                .await
                .unwrap();
            assert!(result.success);
            // The fake ranker reverses [srv__a, srv__b] -> [srv__b, srv__a];
            // max_results=1 keeps only the first post-rerank, so srv__b (not
            // srv__a, the keyword-match order's own first result) must win —
            // proof the rerank output, not just the raw match order, drives
            // what gets returned.
            assert!(
                result.output.contains("srv__b"),
                "reranked top result must be returned: {}",
                result.output
            );
            assert!(!result.output.contains("srv__a"));

            let calls = ranker.rerank_calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "tenant-x");
            assert_eq!(calls[0].2, vec!["srv__seed".to_string()]);
        }

        #[tokio::test]
        async fn keyword_search_records_co_activation_for_surfaced_tools() {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let ranker: Arc<FakeRanker> = Arc::new(FakeRanker::default());
            let tool = ToolSearchTool::new(
                make_deferred_set(vec![
                    make_stub("srv__a", "tool for files"),
                    make_stub("srv__b", "tool for files"),
                ])
                .await,
                Arc::clone(&activated),
            )
            .with_capability_graph("tenant-y".to_string(), Arc::clone(&ranker) as Arc<_>);

            tool.execute(serde_json::json!({"query": "files"}))
                .await
                .unwrap();

            let calls = ranker.co_activation_calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "tenant-y");
            let mut names = calls[0].1.clone();
            names.sort();
            assert_eq!(names, vec!["srv__a".to_string(), "srv__b".to_string()]);
        }

        #[tokio::test]
        async fn select_mode_records_co_activation_for_found_names_only() {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let ranker: Arc<FakeRanker> = Arc::new(FakeRanker::default());
            let tool = ToolSearchTool::new(
                make_deferred_set(vec![make_stub("srv__ok", "OK tool")]).await,
                Arc::clone(&activated),
            )
            .with_capability_graph("tenant-z".to_string(), Arc::clone(&ranker) as Arc<_>);

            let result = tool
                .execute(serde_json::json!({"query": "select:srv__ok,srv__missing"}))
                .await
                .unwrap();
            assert!(result.success);
            assert!(result.output.contains("Not found"));

            let calls = ranker.co_activation_calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "tenant-z");
            assert_eq!(
                calls[0].1,
                vec!["srv__ok".to_string()],
                "only the found name is recorded, not the not-found one"
            );
        }

        #[tokio::test]
        async fn no_ranker_attached_leaves_behavior_unchanged() {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let tool = ToolSearchTool::new(
                make_deferred_set(vec![make_stub("srv__a", "tool for files")]).await,
                activated,
            );
            // No `.with_capability_graph(...)` call at all — must behave
            // exactly like every pre-Phase-4.2 test above.
            let result = tool
                .execute(serde_json::json!({"query": "files"}))
                .await
                .unwrap();
            assert!(result.success);
            assert!(result.output.contains("srv__a"));
        }
    }
}
