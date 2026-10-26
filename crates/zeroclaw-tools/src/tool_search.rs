//! `tool_search` discovery for built-in and MCP tool schemas.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::mcp_deferred::{ActivatedToolSet, DeferredMcpToolSet, deferred_search_score};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, ToolSpec};

/// Default maximum number of search results.
const DEFAULT_MAX_RESULTS: usize = 5;

type ActivationHook = Arc<dyn Fn(Arc<dyn Tool>) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolAccessPolicy {
    pub allowed: Option<Vec<String>>,
    pub caller_allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
}

impl ToolAccessPolicy {
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
/// Fetches full schemas for admitted deferred built-ins and MCP tools.
pub struct ToolSearchTool {
    // This is the executable deferred registry, not a cached grants record.
    // Session admission may only remove entries from it.
    deferred: Mutex<Option<DeferredMcpToolSet>>,
    activated: Arc<Mutex<ActivatedToolSet>>,
    access_policy: Option<ToolAccessPolicy>,
    activation_hook: Option<ActivationHook>,
    #[cfg(feature = "memory-postgres")]
    capability_graph: Option<CapabilityGraphHandle>,
}

impl ToolSearchTool {
    /// The native creator's callable name; ownership is claimed on creation.
    pub const NAME: &'static str = "tool_search";

    pub fn new(deferred: DeferredMcpToolSet, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred: Mutex::new(Some(deferred)),
            activated,
            access_policy: None,
            activation_hook: None,
            #[cfg(feature = "memory-postgres")]
            capability_graph: None,
        }
    }

    /// Built-in schema discovery requires no MCP registry or executable wrapper.
    pub fn for_builtin_schemas(activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred: Mutex::new(None),
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

    /// Remove revoked schemas and activated tools together. Recovering a
    /// poisoned guard is safe here because every surviving entry is checked
    /// against the current ceiling before execution can resume.
    pub fn narrow_to_caller(&self, allowed: &[String]) {
        let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(deferred) = deferred.as_mut() {
            deferred
                .stubs
                .retain(|stub| allowed.contains(&stub.prefixed_name));
        }
        self.activated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain_allowed(allowed);
    }

    /// Advertise only still-loadable, not-yet-activated tools, from the same
    /// registry used by both keyword search and exact selection.
    pub fn deferred_prompt_section(&self) -> String {
        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let activated = self.activated.lock().unwrap_or_else(|e| e.into_inner());
        let names = activated
            .tool_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut section = match self.access_policy.as_ref() {
            Some(policy) => {
                crate::mcp_deferred::build_deferred_builtin_tools_section(&activated, Some(policy))
            }
            None => activated.deferred_builtin_prompt_section(),
        };
        if let Some(deferred) = deferred.as_ref() {
            section.push_str(
                &crate::mcp_deferred::build_deferred_tools_section_excluding(
                    deferred,
                    self.access_policy.as_ref(),
                    &names,
                ),
            );
        }
        section
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
        Self::NAME
    }

    fn description(&self) -> &str {
        "Fetch full schema definitions for deferred built-in and MCP tools. \
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

        self.search_tools(query, max_results).await
    }
}

impl ToolSearchTool {
    /// Select built-in metadata or activate an MCP wrapper. Only MCP tools
    /// enter the executable set and the activation hook's notification list.
    fn load_schema(
        deferred: Option<&DeferredMcpToolSet>,
        activated: &mut ActivatedToolSet,
        name: &str,
        newly_activated: &mut Vec<Arc<dyn Tool>>,
    ) -> Option<ToolSpec> {
        if let Some(spec) = activated.select_builtin_schema(name) {
            activated.mark_loaded_on_demand(name);
            return Some(spec);
        }
        let deferred = deferred?;
        let spec = deferred.tool_spec(name)?;
        if !activated.is_activated(name)
            && let Some(tool) = deferred.activate(name)
        {
            let tool: Arc<dyn Tool> = Arc::from(tool);
            activated.activate_on_demand(name.to_string(), Arc::clone(&tool));
            newly_activated.push(tool);
        } else {
            activated.mark_loaded_on_demand(name);
        }
        Some(spec)
    }

    /// Cerveau: keep the loaded schemas inside their bounds after a call
    /// loaded `returned`, and tell the model what was unloaded so it can
    /// load it again instead of calling a tool that is gone.
    fn enforce_limits(
        activated: &mut ActivatedToolSet,
        returned: &std::collections::HashSet<String>,
        output: &mut String,
    ) {
        let unloaded = activated.enforce_on_demand_limits(returned);
        if unloaded.is_empty() {
            return;
        }
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_attrs(::serde_json::json!({ "unloaded": unloaded })),
            "tool_search unloaded least recently used schemas to stay inside the context budget"
        );
        let _ = write!(
            output,
            "\nUnloaded to stay inside the context budget (least recently used): {}. \
             To use one again, call tool_search with \"select:<name>\" first.",
            unloaded.join(", ")
        );
    }

    async fn search_tools(&self, query: &str, max_results: usize) -> anyhow::Result<ToolResult> {
        // Ranking holds both locks; the capability-graph rerank below awaits, so
        // the std `MutexGuard`s must be out of scope first (they are not `Send`).
        #[cfg_attr(not(feature = "memory-postgres"), allow(unused_mut))]
        let (mut results, recently_activated) = {
            let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            let guard = match self.activated.lock() {
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
            let terms: Vec<_> = query
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect();
            // Rank the combined catalog before capping. Denied names never occupy
            // a result slot. MCP ties preserve the registry's existing order.
            let mut results = Vec::new();
            if let Some(deferred) = deferred.as_ref() {
                for stub in &deferred.stubs {
                    let hits =
                        deferred_search_score(&stub.prefixed_name, &stub.description, &terms);
                    if hits > 0 && self.is_allowed(&stub.prefixed_name) {
                        results.push((stub.prefixed_name.clone(), hits));
                    }
                }
            }
            let mut builtin_specs: Vec<_> = guard.deferred_builtin_specs().collect();
            builtin_specs.sort_by(|a, b| a.name.cmp(&b.name));
            for spec in builtin_specs {
                let hits = deferred_search_score(&spec.name, &spec.description, &terms);
                if hits > 0 && self.is_allowed(&spec.name) {
                    results.push((spec.name.clone(), hits));
                }
            }
            results.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            let recently_activated: Vec<String> = guard
                .tool_specs()
                .iter()
                .map(|spec| spec.name.clone())
                .collect();
            (results, recently_activated)
        };
        #[cfg(not(feature = "memory-postgres"))]
        let _ = recently_activated;

        // Cerveau (Phase 4.2): rerank by learned per-tenant "commonly used
        // together" affinity to whatever's already activated this session.
        // A no-op (order unchanged) when no ranker is attached, or when
        // nothing is activated yet to rank against — see
        // `zeroclaw_memory::capability_graph`'s module docs for scope.
        #[cfg(feature = "memory-postgres")]
        if let Some((tenant_id, ranker)) = &self.capability_graph
            && !recently_activated.is_empty()
            && !results.is_empty()
        {
            let candidates: Vec<String> = results.iter().map(|(name, _)| name.clone()).collect();
            let ranked = ranker
                .rerank(tenant_id, &candidates, &recently_activated)
                .await;
            let mut by_name: std::collections::HashMap<String, usize> = results.drain(..).collect();
            results = ranked
                .into_iter()
                .filter_map(|name| by_name.remove(&name).map(|hits| (name, hits)))
                .collect();
        }
        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching deferred tools found.".into(),
                error: None,
            });
        }
        // Block-scoped so the non-`Send` guards are out of scope before the
        // capability-graph `.await` below.
        #[cfg_attr(not(feature = "memory-postgres"), allow(unused_variables))]
        let (output, newly_activated, returned_names) = {
            let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            let mut guard = self
                .activated
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut output = String::from("<functions>\n");
            let mut newly_activated = Vec::new();
            let mut returned_names = std::collections::HashSet::new();
            for (name, _) in &results {
                if returned_names.len() >= max_results {
                    break;
                }
                if returned_names.contains(name) {
                    continue;
                }
                if let Some(spec) =
                    Self::load_schema(deferred.as_ref(), &mut guard, name, &mut newly_activated)
                {
                    write_schema(&mut output, &spec)?;
                    returned_names.insert(name.clone());
                }
            }
            output.push_str("</functions>\n");
            Self::enforce_limits(&mut guard, &returned_names, &mut output);
            (output, newly_activated, returned_names)
        };
        let activated_count = newly_activated.len();
        self.notify_activated(newly_activated);

        // Cerveau (Phase 4.2): the tools surfaced together by one keyword
        // search are the "used together" proxy signal — strengthens (or
        // creates) a learned edge between every pair. Fire-and-forget: a
        // backend error here is logged and swallowed inside
        // `record_co_activation` itself, never propagated to this call.
        #[cfg(feature = "memory-postgres")]
        if let Some((tenant_id, ranker)) = &self.capability_graph {
            let surfaced: Vec<String> = returned_names.iter().cloned().collect();
            ranker.record_co_activation(tenant_id, &surfaced).await;
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

    async fn select_tools(&self, names: &[&str]) -> anyhow::Result<ToolResult> {
        // Block-scoped so the non-`Send` guards are out of scope before the
        // capability-graph `.await` below.
        #[cfg_attr(not(feature = "memory-postgres"), allow(unused_variables))]
        let (mut output, not_found, newly_activated, returned_names) = {
            let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            let mut output = String::from("<functions>\n");
            let mut not_found = Vec::new();
            let mut newly_activated = Vec::new();
            let mut returned_names = std::collections::HashSet::new();
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
                if name.is_empty() || returned_names.contains(*name) {
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
                match Self::load_schema(deferred.as_ref(), &mut guard, name, &mut newly_activated) {
                    Some(spec) => {
                        write_schema(&mut output, &spec)?;
                        returned_names.insert(*name);
                    }
                    None => {
                        not_found.push(*name);
                    }
                }
            }

            output.push_str("</functions>\n");
            let keep = returned_names
                .iter()
                .map(|name| (*name).to_string())
                .collect();
            Self::enforce_limits(&mut guard, &keep, &mut output);
            (output, not_found, newly_activated, returned_names)
        };
        let activated_count = newly_activated.len();
        self.notify_activated(newly_activated);

        // Cerveau (Phase 4.2): a multi-name `select:` call is an even
        // stronger "these belong together" signal than a keyword search —
        // the caller named them explicitly. Same fail-open contract as the
        // keyword-search path above.
        #[cfg(feature = "memory-postgres")]
        if let Some((tenant_id, ranker)) = &self.capability_graph {
            let found: Vec<String> = returned_names.iter().map(|n| (*n).to_string()).collect();
            ranker.record_co_activation(tenant_id, &found).await;
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

/// Keep the existing function envelope while escaping descriptions as JSON
/// and retaining any declared structured-output schema and parameter domains.
fn write_schema(output: &mut String, spec: &ToolSpec) -> serde_json::Result<()> {
    let _ = write!(
        output,
        "<function>{{\"name\": {}, \"description\": {}, \"parameters\": {}",
        serde_json::to_string(&spec.name)?,
        serde_json::to_string(&spec.description)?,
        spec.parameters
    );
    if let Some(schema) = &spec.output {
        let _ = write!(output, ", \"output\": {schema}");
    }
    if !spec.param_domains.is_empty() {
        let _ = write!(
            output,
            ", \"param_domains\": {}",
            serde_json::to_string(&spec.param_domains)?
        );
    }
    output.push_str("}</function>\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpRegistry;
    use crate::mcp_deferred::{
        DeferredMcpToolStub, build_deferred_tools_section, build_deferred_tools_section_filtered,
    };
    use crate::mcp_protocol::McpToolDef;

    async fn make_deferred_set(stubs: Vec<DeferredMcpToolStub>) -> DeferredMcpToolSet {
        let registry = Arc::new(McpRegistry::connect_all(&[]).await.unwrap());
        DeferredMcpToolSet {
            stubs,
            registry,
            security: Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
        }
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

    fn builtin_spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            description,
            serde_json::json!({
                "type": "object",
                "properties": {"event_date": {"type": "string", "format": "date"}},
                "required": ["event_date"]
            }),
        )
    }

    fn returned_schemas(result: &ToolResult) -> Vec<serde_json::Value> {
        result
            .output
            .lines()
            .filter_map(|line| line.strip_prefix("<function>")?.strip_suffix("</function>"))
            .map(|schema| serde_json::from_str(schema).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn builtin_only_search_returns_full_schema_without_executable_activation() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let mut spec = builtin_spec(
            "calendar",
            "Manage events.\nUse the \"calendar\" path C:\\events.",
        );
        spec.output = Some(serde_json::json!({
            "type": "object", "properties": {"event_id": {"type": "string"}}
        }));
        activated
            .lock()
            .unwrap()
            .set_deferred_builtin_specs(vec![spec.clone()]);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::for_builtin_schemas(Arc::clone(&activated))
            .with_activation_hook(Arc::new(move |tool| {
                seen_hook.lock().unwrap().push(tool.name().to_string());
            }));
        assert!(tool.deferred.lock().unwrap().is_none());
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar - Manage events."));
        assert!(!prompt.contains("event_date"));

        // The full description remains searchable even though the compact
        // prompt advertises only its first line.
        let result = tool
            .execute(serde_json::json!({"query": "C:\\events"}))
            .await
            .unwrap();
        let schemas = returned_schemas(&result);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0], serde_json::to_value(&spec).unwrap());
        let guard = activated.lock().unwrap();
        assert!(guard.is_builtin_schema_exposed("calendar"));
        assert!(!guard.is_activated("calendar"));
        assert!(guard.get("calendar").is_none());
        assert!(guard.get_resolved("calendar").is_none());
        assert!(guard.tool_names().is_empty());
        assert!(guard.tool_specs().is_empty());
        drop(guard);
        assert!(seen.lock().unwrap().is_empty());
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn builtin_exact_selection_preserves_explicit_names_and_recovers_poisoned_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar events"),
            builtin_spec("weather", "Weather forecast"),
        ]);
        let poison = Arc::clone(&activated);
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("test poison");
            })
            .join()
            .is_err()
        );
        let tool = ToolSearchTool::for_builtin_schemas(Arc::clone(&activated));
        let result = tool
            .execute(serde_json::json!({
                "query": "select:calendar,calendar,weather", "max_results": 1
            }))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 2);
        {
            let guard = activated.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.is_builtin_schema_exposed("calendar"));
            assert!(guard.is_builtin_schema_exposed("weather"));
            assert!(guard.tool_specs().is_empty());
        }
        assert!(tool.deferred_prompt_section().is_empty());
        let result = tool
            .execute(serde_json::json!({"query": "select:weather"}))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result)[0]["name"], "weather");
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn mixed_search_and_selection_share_result_limit_and_only_activate_mcp() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar events search"),
            builtin_spec("weather", "Weather search"),
        ]);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("remote__calendar", "Calendar search")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            seen_hook.lock().unwrap().push(tool.name().to_string());
        }));
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar - Calendar events search"));
        assert!(prompt.contains("remote__calendar - Calendar search"));
        let result = tool
            .execute(serde_json::json!({"query": "calendar events search", "max_results": 2}))
            .await
            .unwrap();
        let schemas = returned_schemas(&result);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0]["name"], "calendar");
        assert_eq!(schemas[1]["name"], "remote__calendar");
        {
            let guard = activated.lock().unwrap();
            assert!(guard.is_builtin_schema_exposed("calendar"));
            assert!(!guard.is_builtin_schema_exposed("weather"));
            assert!(guard.is_activated("remote__calendar"));
            assert_eq!(guard.tool_names(), vec!["remote__calendar"]);
        }
        assert_eq!(seen.lock().unwrap().as_slice(), ["remote__calendar"]);

        let result = tool
            .execute(serde_json::json!({
                "query": "select:weather,remote__calendar,calendar", "max_results": 1
            }))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 3);
        assert_eq!(returned_schemas(&result)[0]["name"], "weather");
        assert_eq!(seen.lock().unwrap().as_slice(), ["remote__calendar"]);
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn builtin_policy_and_principal_narrowing_remove_discovery_and_selection() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar tool"),
            builtin_spec("weather", "Weather tool"),
            builtin_spec("blocked", "Blocked tool"),
            builtin_spec("outside", "Outside caller tool"),
            builtin_spec("risk_blocked", "Outside risk tool"),
        ]);
        let policy = ToolAccessPolicy {
            allowed: Some(vec![
                "calendar".into(),
                "weather".into(),
                "blocked".into(),
                "outside".into(),
            ]),
            caller_allowed: Some(vec![
                "calendar".into(),
                "weather".into(),
                "blocked".into(),
                "risk_blocked".into(),
            ]),
            denied: Some(vec!["blocked".into()]),
        };
        let tool =
            ToolSearchTool::for_builtin_schemas(Arc::clone(&activated)).with_access_policy(policy);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar"));
        for denied in ["blocked", "outside", "risk_blocked"] {
            assert!(!prompt.contains(denied));
        }
        let result = tool
            .execute(serde_json::json!({"query": "tool", "max_results": 1}))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 1);
        assert_eq!(returned_schemas(&result)[0]["name"], "calendar");
        let denied = tool
            .execute(serde_json::json!({"query": "select:blocked,outside,risk_blocked"}))
            .await
            .unwrap();
        assert!(returned_schemas(&denied).is_empty());
        assert!(
            !activated
                .lock()
                .unwrap()
                .is_builtin_schema_exposed("blocked")
        );

        tool.narrow_to_caller(&["weather".to_string()]);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("weather"));
        assert!(!prompt.contains("calendar"));
        for query in ["calendar", "select:calendar"] {
            let result = tool
                .execute(serde_json::json!({"query": query}))
                .await
                .unwrap();
            assert!(returned_schemas(&result).is_empty());
        }
        let guard = activated.lock().unwrap();
        assert_eq!(
            guard.hidden_builtin_names(),
            std::collections::HashSet::from(["weather".to_string()])
        );
        assert!(guard.tool_specs().is_empty());
    }

    #[tokio::test]
    async fn principal_narrowing_keeps_positive_prompt_and_blocks_reactivation() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![
                make_stub("mcp__keep", "retained tool"),
                make_stub("mcp__revoke", "revoked tool"),
            ])
            .await,
            Arc::clone(&activated),
        );
        assert!(tool.deferred_prompt_section().contains("mcp__revoke"));
        tool.execute(serde_json::json!({"query":"select:mcp__revoke"}))
            .await
            .unwrap();
        assert!(activated.lock().unwrap().is_activated("mcp__revoke"));
        let poison = Arc::clone(&activated);
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("test poison");
            })
            .join()
            .is_err()
        );
        tool.narrow_to_caller(&["mcp__keep".into()]);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("mcp__keep") && prompt.contains("tool_search"));
        assert!(!prompt.contains("mcp__revoke"));
        for query in ["select:mcp__revoke", "revoked"] {
            let result = tool
                .execute(serde_json::json!({"query":query}))
                .await
                .unwrap();
            assert!(!result.output.contains("<function>{"));
            assert!(
                !activated
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_activated("mcp__revoke")
            );
        }
        tool.execute(serde_json::json!({"query":"select:mcp__keep"}))
            .await
            .unwrap();
        assert!(
            activated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_activated("mcp__keep")
        );
        assert!(tool.deferred_prompt_section().is_empty());
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
        // See `ToolAccessPolicy::is_tool_allowed` and
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

    #[tokio::test]
    async fn prompt_section_and_tool_search_see_same_filtered_set() {
        // The prompt-side
        // `build_deferred_tools_section_filtered` and the runtime
        // `ToolSearchTool` constructor (built from the same
        // `filtered_deferred` value, the single source of
        // truth) must agree on which tools are visible. If the
        // prompt and the search tool each applied the access policy
        // independently, a denied tool could leak through one of them.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once and
        // feed it to both consumers.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: the section must not mention the denied tool.
        let section = build_deferred_tools_section_filtered(&filtered, Some(&policy));
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: the search tool must not return the denied
        // tool's schema on a keyword match.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated)).with_access_policy(policy);
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

    #[tokio::test]
    async fn omission_safety_pre_filtered_set_suffices_without_reapplied_policy() {
        // The `filter_by_policy` helper alone (the single source of
        // truth) is sufficient to keep a denied tool schema out of
        // both consumers, even when the policy is NOT reapplied at the
        // consumer level. A missed `with_access_policy` builder step
        // on `ToolSearchTool` is therefore not load-bearing.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: use the UNFILTERED prompt builder — the pre-filtered
        // set alone must keep the denied tool out. No policy re-applied.
        let section = build_deferred_tools_section(&filtered);
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: construct ToolSearchTool WITHOUT with_access_policy.
        // The pre-filtered stub set alone must keep the denied schema out.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated));
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

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

    #[test]
    fn empty_caller_allowlist_denies_every_mcp_tool() {
        // The principal tool narrowing flows in as `caller_allowed`. An
        // empty principal list must yield a tool-less session even in the
        // presence of MCP `<server>__<tool>` names, which the risk-profile
        // gate would otherwise auto-admit. No `__` escape at the caller layer.
        let policy = ToolAccessPolicy::from_security(None, None, Some(&[]))
            .expect("an empty caller allowlist still produces a policy");
        assert!(!policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "empty caller allowlist must deny MCP tools despite the __ auto-admit"
        );
    }

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
            let seed_tool: Arc<dyn Tool> = Arc::new(make_stub("srv__seed", "seed tool").activate(
                Arc::new(McpRegistry::connect_all(&[]).await.unwrap()),
                Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
            ));
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

    #[tokio::test]
    async fn select_past_the_bound_unloads_the_oldest_schema_and_says_so() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::with_limits(2, usize::MAX)));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("alpha", "first"),
            builtin_spec("beta", "second"),
            builtin_spec("gamma", "third"),
        ]);
        let tool = ToolSearchTool::for_builtin_schemas(Arc::clone(&activated));
        let first = tool
            .execute(serde_json::json!({"query": "select:alpha,beta"}))
            .await
            .unwrap();
        assert!(!first.output.to_string().contains("Unloaded"));
        let second = tool
            .execute(serde_json::json!({"query": "select:gamma"}))
            .await
            .unwrap();
        let text = second.output.to_string();
        assert!(
            text.contains("Unloaded to stay inside the context budget"),
            "{text}"
        );
        assert!(text.contains("alpha"), "{text}");
        let guard = activated.lock().unwrap();
        assert!(!guard.is_builtin_schema_exposed("alpha"));
        assert!(guard.is_builtin_schema_exposed("beta"));
        assert!(guard.is_builtin_schema_exposed("gamma"));
    }
}
