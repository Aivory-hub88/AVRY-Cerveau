//! Deferred MCP tool loading — stubs and activated-tool tracking.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::mcp_client::McpRegistry;
use crate::mcp_protocol::McpToolDef;
use crate::mcp_tool::McpToolWrapper;
use crate::tool_search::ToolAccessPolicy;
use zeroclaw_api::tool::{Tool, ToolSpec};
use zeroclaw_config::policy::SecurityPolicy;

// ── DeferredMcpToolStub ──────────────────────────────────────────────────

/// A lightweight stub representing a known-but-not-yet-loaded MCP tool.
/// Contains only the prefixed name, a human-readable description, and enough
/// information to construct the full [`McpToolWrapper`] on activation.
#[derive(Debug, Clone)]
pub struct DeferredMcpToolStub {
    /// Prefixed name: `<server_name>__<tool_name>`.
    pub prefixed_name: String,
    /// Human-readable description (extracted from the MCP tool definition).
    pub description: String,
    /// The full tool definition — stored so we can construct a wrapper later.
    def: McpToolDef,
}

impl DeferredMcpToolStub {
    pub fn new(prefixed_name: String, def: McpToolDef) -> Self {
        let description = def
            .description
            .clone()
            .unwrap_or_else(|| "MCP tool".to_string());
        Self {
            prefixed_name,
            description,
            def,
        }
    }

    /// Materialize this stub into a live [`McpToolWrapper`].
    ///
    /// `security` is the execution-scope [`SecurityPolicy`] snapshot (source of
    /// truth for the workspace path used when materializing MCP `resource`+`blob`
    /// results) — an immutable `Arc`, not a reloadable live handle.
    pub fn activate(
        &self,
        registry: Arc<McpRegistry>,
        security: Arc<SecurityPolicy>,
    ) -> McpToolWrapper {
        McpToolWrapper::new(
            self.prefixed_name.clone(),
            self.def.clone(),
            registry,
            security,
        )
    }
}

// ── DeferredMcpToolSet ───────────────────────────────────────────────────

/// Collection of all deferred MCP tool stubs discovered at startup.
/// Provides keyword search for `tool_search`.
#[derive(Clone)]
pub struct DeferredMcpToolSet {
    /// All stubs — exposed for test construction.
    pub stubs: Vec<DeferredMcpToolStub>,
    /// Shared registry — exposed for test construction.
    pub registry: Arc<McpRegistry>,
    /// Security policy handle for activated wrappers (workspace at use time).
    pub security: Arc<SecurityPolicy>,
}

impl DeferredMcpToolSet {
    /// Build the set from a connected [`McpRegistry`].
    pub async fn from_registry(registry: Arc<McpRegistry>, security: Arc<SecurityPolicy>) -> Self {
        let names = registry.tool_names();
        let mut stubs = Vec::with_capacity(names.len());
        for name in names {
            if let Some(def) = registry.get_tool_def(&name).await {
                stubs.push(DeferredMcpToolStub::new(name, def));
            }
        }
        Self {
            stubs,
            registry,
            security,
        }
    }

    /// All stub names (for rendering in the system prompt).
    pub fn stub_names(&self) -> Vec<&str> {
        self.stubs
            .iter()
            .map(|s| s.prefixed_name.as_str())
            .collect()
    }

    /// Number of deferred stubs.
    pub fn len(&self) -> usize {
        self.stubs.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.stubs.is_empty()
    }

    /// Return a copy of this set with stubs whose `prefixed_name` is
    /// not allowed by `policy` removed. `policy == None` is a no-op
    /// (the set is returned unchanged) — this matches the
    /// `if let Some(policy) = ...` pattern at every production
    /// call site. Centralizes the per-stub filter logic so the
    /// prompt-side claim and the `ToolSearchTool` constructor see
    /// exactly the same tool set.
    pub fn filter_by_policy(&self, policy: Option<&ToolAccessPolicy>) -> Self {
        let filtered_stubs: Vec<DeferredMcpToolStub> = self
            .stubs
            .iter()
            .filter(|stub| policy.is_none_or(|p| p.is_tool_allowed(&stub.prefixed_name)))
            .cloned()
            .collect();
        Self {
            stubs: filtered_stubs,
            registry: Arc::clone(&self.registry),
            security: Arc::clone(&self.security),
        }
    }

    /// Look up stubs by exact name. Used for `select:name1,name2` queries.
    pub fn get_by_name(&self, name: &str) -> Option<&DeferredMcpToolStub> {
        self.stubs.iter().find(|s| s.prefixed_name == name)
    }

    /// Keyword search — returns stubs whose name or description contains any
    /// of the query terms (case-insensitive). Results are ranked by number of
    /// matching terms (descending).
    pub fn search(&self, query: &str, max_results: usize) -> Vec<&DeferredMcpToolStub> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        if terms.is_empty() {
            return self.stubs.iter().take(max_results).collect();
        }

        let mut scored: Vec<(&DeferredMcpToolStub, usize)> = self
            .stubs
            .iter()
            .filter_map(|stub| {
                let hits = deferred_search_score(&stub.prefixed_name, &stub.description, &terms);
                if hits > 0 { Some((stub, hits)) } else { None }
            })
            .collect();

        scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        scored
            .into_iter()
            .take(max_results)
            .map(|(s, _)| s)
            .collect()
    }

    /// Activate a stub by name, returning a boxed [`Tool`].
    pub fn activate(&self, name: &str) -> Option<Box<dyn Tool>> {
        self.get_by_name(name).map(|stub| {
            let wrapper = stub.activate(Arc::clone(&self.registry), Arc::clone(&self.security));
            Box::new(wrapper) as Box<dyn Tool>
        })
    }

    /// Return the full [`ToolSpec`] for a stub (for inclusion in `tool_search` results).
    pub fn tool_spec(&self, name: &str) -> Option<ToolSpec> {
        self.get_by_name(name).map(|stub| {
            let wrapper = stub.activate(Arc::clone(&self.registry), Arc::clone(&self.security));
            wrapper.spec()
        })
    }
}

/// Shared keyword ranking for MCP stubs and built-in schema metadata.
pub(crate) fn deferred_search_score(name: &str, description: &str, terms: &[String]) -> usize {
    let haystack = format!("{name} {description}").to_ascii_lowercase();
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count()
}

// ── ActivatedToolSet ─────────────────────────────────────────────────────

/// Default number of schemas `tool_search` may keep loaded at once.
pub const DEFAULT_MAX_ON_DEMAND_SCHEMAS: usize = 8;
/// Default combined size of those schemas (name, description and parameter
/// JSON, in chars; about 6k tokens). Whichever bound is hit first applies.
pub const DEFAULT_MAX_ON_DEMAND_SCHEMA_CHARS: usize = 24_000;

/// Per-conversation MCP activation and built-in schema selection.
///
/// Built-in specs are a metadata projection of the scoped registry. The
/// registry remains their executable owner and refreshes this catalog when
/// admitted tools change. Selecting a built-in schema grants no capability.
///
/// Cerveau: schemas loaded on demand by `tool_search` are bounded. Every
/// loaded schema rides on every later provider request of the turn, so an
/// agent that searched widely used to grow its request until the context
/// budget hard-failed the turn (ADR-021 rollout). Past
/// [`DEFAULT_MAX_ON_DEMAND_SCHEMAS`] or [`DEFAULT_MAX_ON_DEMAND_SCHEMA_CHARS`]
/// the least recently loaded or called one is unloaded; `tool_search` says
/// which, and a `select:` loads it again. Tools installed with
/// [`Self::activate`] (the `always` filter groups) are pinned and never count.
pub struct ActivatedToolSet {
    // Executable activation remains MCP-only.
    tools: HashMap<String, Arc<dyn Tool>>,
    deferred_builtin_specs: HashMap<String, ToolSpec>,
    selected_builtin_names: HashSet<String>,
    /// Names loaded on demand, with the tick of their last load or call.
    on_demand: HashMap<String, u64>,
    tick: u64,
    max_on_demand: usize,
    max_on_demand_chars: usize,
}

impl ActivatedToolSet {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_ON_DEMAND_SCHEMAS,
            DEFAULT_MAX_ON_DEMAND_SCHEMA_CHARS,
        )
    }

    /// A set whose on-demand schemas are bounded by count and combined size.
    pub fn with_limits(max_on_demand: usize, max_on_demand_chars: usize) -> Self {
        Self {
            tools: HashMap::new(),
            deferred_builtin_specs: HashMap::new(),
            selected_builtin_names: HashSet::new(),
            on_demand: HashMap::new(),
            tick: 0,
            max_on_demand: max_on_demand.max(1),
            max_on_demand_chars,
        }
    }

    /// Install the initial metadata-only catalog of admitted deferred built-ins.
    pub fn set_deferred_builtin_specs(&mut self, specs: Vec<ToolSpec>) {
        self.deferred_builtin_specs = specs
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect();
        self.selected_builtin_names.clear();
        self.on_demand
            .retain(|name, _| self.tools.contains_key(name));
    }

    /// Refresh previously deferred names from the canonical scoped registry.
    /// New names stay eager; removed names lose their schema and selection.
    pub fn refresh_deferred_builtin_specs(&mut self, specs: Vec<ToolSpec>) {
        self.deferred_builtin_specs = specs
            .into_iter()
            .filter(|spec| self.deferred_builtin_specs.contains_key(&spec.name))
            .map(|spec| (spec.name.clone(), spec))
            .collect();
        self.selected_builtin_names
            .retain(|name| self.deferred_builtin_specs.contains_key(name));
        self.retain_on_demand();
    }

    pub fn hidden_builtin_names(&self) -> HashSet<String> {
        self.deferred_builtin_specs
            .keys()
            .filter(|name| !self.selected_builtin_names.contains(*name))
            .cloned()
            .collect()
    }

    /// Render the current unselected built-in catalog after registry refresh
    /// or principal narrowing, rather than retaining a startup prompt snapshot.
    pub fn deferred_builtin_prompt_section(&self) -> String {
        build_deferred_builtin_tools_section(self, None)
    }

    /// Render discovery for one request without narrowing shared schema metadata.
    pub fn deferred_builtin_prompt_section_filtered(
        &self,
        is_allowed: impl Fn(&str) -> bool,
    ) -> String {
        render_deferred_builtin_tools_section(self, is_allowed)
    }

    pub fn has_deferred_builtin_schemas(&self) -> bool {
        !self.deferred_builtin_specs.is_empty()
    }

    pub fn is_builtin_schema_exposed(&self, name: &str) -> bool {
        !self.deferred_builtin_specs.contains_key(name)
            || self.selected_builtin_names.contains(name)
    }

    pub(crate) fn deferred_builtin_specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.deferred_builtin_specs.values()
    }

    pub(crate) fn select_builtin_schema(&mut self, name: &str) -> Option<ToolSpec> {
        let spec = self.deferred_builtin_specs.get(name)?.clone();
        self.selected_builtin_names.insert(name.to_string());
        Some(spec)
    }

    /// Install an executable that stays loaded (pinned): it never counts
    /// against, nor is unloaded by, the on-demand bounds.
    pub fn activate(&mut self, name: String, tool: Arc<dyn Tool>) {
        self.tools.insert(name, tool);
    }

    /// Install an executable that `tool_search` loaded on demand: it counts
    /// against the bounds and may be unloaded again.
    pub fn activate_on_demand(&mut self, name: String, tool: Arc<dyn Tool>) {
        self.tick += 1;
        self.on_demand.insert(name.clone(), self.tick);
        self.tools.insert(name, tool);
    }

    /// Record that `tool_search` loaded (or loaded again) `name`: a selected
    /// built-in or an on-demand tool. A pinned tool stays pinned.
    pub fn mark_loaded_on_demand(&mut self, name: &str) {
        let pinned = self.tools.contains_key(name) && !self.on_demand.contains_key(name);
        if pinned {
            return;
        }
        self.tick += 1;
        self.on_demand.insert(name.to_string(), self.tick);
    }

    /// Record a call, so a tool in use is the last to be unloaded.
    pub fn note_used(&mut self, name: &str) {
        if let Some(last) = self.on_demand.get_mut(name) {
            self.tick += 1;
            *last = self.tick;
        }
    }

    /// Unload the least recently used on-demand schemas until both bounds
    /// hold, never one named in `keep` (what the current `tool_search` call
    /// returned). Returns the unloaded names, oldest first. An unloaded MCP
    /// tool leaves the executable set; an unloaded built-in only goes back to
    /// being hidden, since the registry still owns it.
    pub fn enforce_on_demand_limits(&mut self, keep: &HashSet<String>) -> Vec<String> {
        let mut unloaded = Vec::new();
        while self.on_demand.len() > self.max_on_demand
            || self.on_demand_chars() > self.max_on_demand_chars
        {
            let Some(oldest) = self
                .on_demand
                .iter()
                .filter(|(name, _)| !keep.contains(*name))
                .min_by_key(|(_, tick)| **tick)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            self.on_demand.remove(&oldest);
            self.tools.remove(&oldest);
            self.selected_builtin_names.remove(&oldest);
            unloaded.push(oldest);
        }
        unloaded
    }

    /// Names currently loaded on demand (not pinned).
    pub fn on_demand_names(&self) -> Vec<&str> {
        self.on_demand.keys().map(String::as_str).collect()
    }

    fn on_demand_chars(&self) -> usize {
        self.on_demand
            .keys()
            .filter_map(|name| {
                self.tools
                    .get(name)
                    .map(|tool| tool.spec())
                    .or_else(|| self.deferred_builtin_specs.get(name).cloned())
            })
            .map(|spec| {
                spec.name.len() + spec.description.len() + spec.parameters.to_string().len()
            })
            .sum()
    }

    fn retain_on_demand(&mut self) {
        let (tools, selected) = (&self.tools, &self.selected_builtin_names);
        self.on_demand
            .retain(|name, _| tools.contains_key(name) || selected.contains(name));
    }

    pub fn is_activated(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Clone the Arc so the caller can drop the mutex guard before awaiting.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn get_resolved(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.get(name) {
            return Some(tool);
        }
        if name.contains("__") {
            return None;
        }

        let mut resolved = None;
        for (tool_name, tool) in &self.tools {
            let Some((_, suffix)) = tool_name.split_once("__") else {
                continue;
            };
            if suffix != name {
                continue;
            }
            if resolved.is_some() {
                return None;
            }
            resolved = Some(Arc::clone(tool));
        }

        resolved
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Remove MCP executables and built-in schema metadata outside a narrowed
    /// principal's ceiling. Callers resolve policy from its canonical source;
    /// this set retains no independent policy.
    pub fn retain_allowed(&mut self, allowed: &[String]) {
        self.tools
            .retain(|name, _| allowed.iter().any(|allowed_name| allowed_name == name));
        self.deferred_builtin_specs
            .retain(|name, _| allowed.contains(name));
        self.selected_builtin_names
            .retain(|name| self.deferred_builtin_specs.contains_key(name));
        self.retain_on_demand();
    }
}

impl Default for ActivatedToolSet {
    fn default() -> Self {
        Self::new()
    }
}

// ── System prompt helper ─────────────────────────────────────────────────

/// Longest one-line summary the deferred index carries per tool, in chars.
const DEFERRED_SUMMARY_MAX_CHARS: usize = 200;

/// The one-line summary the index shows for a deferred tool: the first
/// non-empty line of its description, capped at
/// [`DEFERRED_SUMMARY_MAX_CHARS`] on a char boundary.
///
/// MCP servers routinely ship multi-paragraph docstrings with `Args:` and
/// `Returns:` blocks. Those still drive `tool_search` keyword matching through
/// the full [`DeferredMcpToolStub::description`], but they have no place in a
/// prompt section whose only job is to say which names exist: with a couple
/// of hundred deferred tools the bodies alone cost more context per turn than
/// every activated schema combined.
fn deferred_summary_line(description: &str) -> String {
    summary_line(description, DEFERRED_SUMMARY_MAX_CHARS)
}

fn summary_line(description: &str, max_chars: usize) -> String {
    let line = description
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(max_chars).collect();
    cut.push_str("...");
    cut
}

/// Advertise unselected built-in schemas without copying their JSON schemas
/// into the prompt. Access policy is applied before any name is rendered.
pub(crate) fn build_deferred_builtin_tools_section(
    activated: &ActivatedToolSet,
    policy: Option<&ToolAccessPolicy>,
) -> String {
    render_deferred_builtin_tools_section(activated, |name| {
        policy.is_none_or(|p| p.is_tool_allowed(name))
    })
}

fn render_deferred_builtin_tools_section(
    activated: &ActivatedToolSet,
    is_allowed: impl Fn(&str) -> bool,
) -> String {
    let mut specs: Vec<_> = activated
        .deferred_builtin_specs()
        .filter(|spec| !activated.is_builtin_schema_exposed(&spec.name))
        .filter(|spec| is_allowed(&spec.name))
        .collect();
    if specs.is_empty() {
        return String::new();
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = String::from(
        "## Deferred Built-in Tools\n\n\
         Use `tool_search` to fetch full schemas for the built-in tools below. \
         Use `\"select:name1,name2\"` for exact tools or keywords to search. \
         Schema selection does not change tool permissions or approval requirements.\n\n\
         <available-deferred-builtin-tools>\n",
    );
    for spec in specs {
        out.push_str(&spec.name);
        out.push_str(" - ");
        out.push_str(&summary_line(&spec.description, 120));
        out.push('\n');
    }
    out.push_str("</available-deferred-builtin-tools>\n");
    out
}

/// Build the `<available-deferred-tools>` section for the system prompt.
/// Lists tool names with a one-line summary each (the first non-empty line
/// of the description, capped) so the LLM knows what is available without
/// consuming context window on full schemas or full docstrings. Includes an
/// instruction block that tells the LLM to call `tool_search` to activate
/// them.
pub fn build_deferred_tools_section(deferred: &DeferredMcpToolSet) -> String {
    build_deferred_tools_section_filtered(deferred, None)
}

pub fn build_deferred_tools_section_filtered(
    deferred: &DeferredMcpToolSet,
    policy: Option<&crate::tool_search::ToolAccessPolicy>,
) -> String {
    build_deferred_tools_section_excluding(deferred, policy, &HashSet::new())
}

pub fn build_deferred_tools_section_excluding(
    deferred: &DeferredMcpToolSet,
    policy: Option<&crate::tool_search::ToolAccessPolicy>,
    exclude: &HashSet<String>,
) -> String {
    if deferred.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("## Deferred Tools\n\n");
    out.push_str(
        "The tools listed below are available but NOT yet loaded. \
         To use any of them you MUST first call the `tool_search` tool \
         to fetch their full schemas. Use `\"select:name1,name2\"` for \
         exact tools or keywords to search. Once activated, the tools \
         become callable for the rest of the conversation.\n\n",
    );
    out.push_str("<available-deferred-tools>\n");
    let mut count = 0;
    for stub in &deferred.stubs {
        if exclude.contains(&stub.prefixed_name) {
            continue;
        }
        if let Some(p) = policy
            && !p.is_tool_allowed(&stub.prefixed_name)
        {
            continue;
        }
        out.push_str(&stub.prefixed_name);
        out.push_str(" - ");
        out.push_str(&deferred_summary_line(&stub.description));
        out.push('\n');
        count += 1;
    }
    out.push_str("</available-deferred-tools>\n");
    if count == 0 {
        return String::new();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn make_stub(name: &str, desc: &str) -> DeferredMcpToolStub {
        let def = McpToolDef {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        DeferredMcpToolStub::new(name.to_string(), def)
    }

    #[test]
    fn stub_uses_description_from_def() {
        let stub = make_stub("fs__read", "Read a file");
        assert_eq!(stub.description, "Read a file");
    }

    #[test]
    fn stub_defaults_description_when_none() {
        let def = McpToolDef {
            name: "mystery".into(),
            description: None,
            input_schema: serde_json::json!({}),
        };
        let stub = DeferredMcpToolStub::new("srv__mystery".into(), def);
        assert_eq!(stub.description, "MCP tool");
    }

    #[test]
    fn builtin_catalog_refresh_updates_only_surviving_deferred_names() {
        let make_spec = |name: &str, description: &str| {
            ToolSpec::new(name, description, serde_json::json!({"type": "object"}))
        };
        let mut set = ActivatedToolSet::new();
        set.set_deferred_builtin_specs(vec![
            make_spec("keep", "Original description"),
            make_spec("revoke", "Revoked description"),
            make_spec("hidden", "Unselected description"),
        ]);
        assert!(set.has_deferred_builtin_schemas());
        assert!(!set.is_builtin_schema_exposed("keep"));
        assert!(set.is_builtin_schema_exposed("eager"));
        set.select_builtin_schema("keep").unwrap();
        set.select_builtin_schema("revoke").unwrap();

        set.refresh_deferred_builtin_specs(vec![
            make_spec("keep", "Rebound description"),
            make_spec("hidden", "Updated unselected description"),
            make_spec("new", "New tool stays eager"),
        ]);
        assert!(set.is_builtin_schema_exposed("keep"));
        assert_eq!(
            set.hidden_builtin_names(),
            HashSet::from(["hidden".to_string()])
        );
        assert_eq!(
            set.select_builtin_schema("keep").unwrap().description,
            "Rebound description"
        );
        assert!(set.select_builtin_schema("revoke").is_none());
        assert!(set.select_builtin_schema("new").is_none());
        assert!(set.selected_builtin_names.contains("keep"));
        assert!(!set.selected_builtin_names.contains("revoke"));
        assert!(set.get("keep").is_none());
        assert!(set.get_resolved("keep").is_none());
        assert!(set.tool_names().is_empty());
        assert!(set.tool_specs().is_empty());

        set.retain_allowed(&[]);
        assert!(!set.has_deferred_builtin_schemas());
        assert!(set.hidden_builtin_names().is_empty());
        assert!(set.selected_builtin_names.is_empty());
        assert!(set.deferred_builtin_prompt_section().is_empty());
        // A later refresh cannot revive a revoked schema.
        set.refresh_deferred_builtin_specs(vec![make_spec("keep", "Reintroduced")]);
        assert!(set.select_builtin_schema("keep").is_none());
    }

    #[test]
    fn builtin_prompt_is_compact_filtered_and_omits_selected_schemas() {
        let mut set = ActivatedToolSet::new();
        set.set_deferred_builtin_specs(vec![
            ToolSpec::new(
                "calendar",
                format!("\n  {}\nArgs: full documentation", "é".repeat(150)),
                serde_json::json!({"properties": {"private_schema_marker": {"type": "string"}}}),
            ),
            ToolSpec::new("denied", "Denied description", serde_json::json!({})),
        ]);
        let policy = ToolAccessPolicy {
            denied: Some(vec!["denied".to_string()]),
            ..ToolAccessPolicy::default()
        };
        let prompt = build_deferred_builtin_tools_section(&set, Some(&policy));
        assert!(prompt.contains("tool_search"));
        assert!(prompt.contains("select:name1,name2"));
        assert!(prompt.contains(&format!("calendar - {}...\n", "é".repeat(120))));
        assert!(!prompt.contains("Args:"));
        assert!(!prompt.contains("private_schema_marker"));
        assert!(!prompt.contains("denied"));
        let hidden_names = set.hidden_builtin_names();
        assert_eq!(
            set.deferred_builtin_prompt_section_filtered(|name| name == "calendar"),
            prompt
        );
        assert!(
            set.deferred_builtin_prompt_section_filtered(|_| false)
                .is_empty()
        );
        assert_eq!(set.hidden_builtin_names(), hidden_names);
        set.select_builtin_schema("calendar").unwrap();
        assert!(build_deferred_builtin_tools_section(&set, Some(&policy)).is_empty());
    }

    #[test]
    fn activated_set_tracks_activation() {
        use async_trait::async_trait;
        use zeroclaw_api::tool::{ToolOutput, ToolResult};

        struct FakeTool;
        impl ::zeroclaw_api::attribution::Attributable for FakeTool {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Tool(
                    ::zeroclaw_api::attribution::ToolKind::Plugin,
                )
            }
            fn alias(&self) -> &str {
                <Self as Tool>::name(self)
            }
        }
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                "fake"
            }
            fn description(&self) -> &str {
                "fake tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: ToolOutput::default(),
                    error: None,
                })
            }
        }

        let mut set = ActivatedToolSet::new();
        assert!(!set.is_activated("fake"));
        set.activate("fake".into(), Arc::new(FakeTool));
        assert!(set.is_activated("fake"));
        assert!(set.get("fake").is_some());
        assert_eq!(set.tool_specs().len(), 1);
    }

    #[test]
    fn activated_set_resolves_unique_suffix() {
        use async_trait::async_trait;
        use zeroclaw_api::tool::{ToolOutput, ToolResult};

        struct FakeTool;
        impl ::zeroclaw_api::attribution::Attributable for FakeTool {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Tool(
                    ::zeroclaw_api::attribution::ToolKind::Plugin,
                )
            }
            fn alias(&self) -> &str {
                <Self as Tool>::name(self)
            }
        }
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                "docker-mcp__extract_text"
            }
            fn description(&self) -> &str {
                "fake tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: ToolOutput::default(),
                    error: None,
                })
            }
        }

        let mut set = ActivatedToolSet::new();
        set.activate("docker-mcp__extract_text".into(), Arc::new(FakeTool));
        assert!(set.get_resolved("extract_text").is_some());
    }

    #[test]
    fn activated_set_rejects_ambiguous_suffix() {
        use async_trait::async_trait;
        use zeroclaw_api::tool::{ToolOutput, ToolResult};

        struct FakeTool(&'static str);
        impl ::zeroclaw_api::attribution::Attributable for FakeTool {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Tool(
                    ::zeroclaw_api::attribution::ToolKind::Plugin,
                )
            }
            fn alias(&self) -> &str {
                <Self as Tool>::name(self)
            }
        }
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: ToolOutput::default(),
                    error: None,
                })
            }
        }

        let mut set = ActivatedToolSet::new();
        set.activate(
            "docker-mcp__extract_text".into(),
            Arc::new(FakeTool("docker-mcp__extract_text")),
        );
        set.activate(
            "ocr-mcp__extract_text".into(),
            Arc::new(FakeTool("ocr-mcp__extract_text")),
        );
        assert!(set.get_resolved("extract_text").is_none());
    }

    #[test]
    fn build_deferred_section_empty_when_no_stubs() {
        let set = DeferredMcpToolSet {
            stubs: vec![],
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        assert!(build_deferred_tools_section(&set).is_empty());
    }

    #[test]
    fn build_deferred_section_lists_names() {
        let stubs = vec![
            make_stub("fs__read_file", "Read a file"),
            make_stub("git__status", "Git status"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        let section = build_deferred_tools_section(&set);
        assert!(section.contains("<available-deferred-tools>"));
        assert!(section.contains("fs__read_file - Read a file"));
        assert!(section.contains("git__status - Git status"));
        assert!(section.contains("</available-deferred-tools>"));
    }

    fn make_set(stubs: Vec<DeferredMcpToolStub>) -> DeferredMcpToolSet {
        DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        }
    }

    /// A docstring-style description contributes only its first line to the
    /// index; the `Args:` and `Returns:` body must not reach the prompt.
    #[test]
    fn build_deferred_section_uses_first_line_of_multiline_description() {
        let desc = "Fetch the food diary for a day.\n\n    Args:\n        date: YYYY-MM-DD, defaults to today.\n\n    Returns:\n        Every entry logged for the day.";
        let set = make_set(vec![make_stub("cronometer__get_diary", desc)]);
        let section = build_deferred_tools_section(&set);
        assert!(section.contains("cronometer__get_diary - Fetch the food diary for a day.\n"));
        assert!(!section.contains("Args:"));
        assert!(!section.contains("Returns:"));
        assert!(!section.contains("defaults to today"));
    }

    /// Leading blank lines and indentation are skipped so a description that
    /// starts with a newline still yields its real first sentence.
    #[test]
    fn build_deferred_section_skips_leading_blank_lines() {
        let set = make_set(vec![make_stub(
            "wger__get_routine",
            "\n\n   Fetch a routine.  \nMore.",
        )]);
        let section = build_deferred_tools_section(&set);
        assert!(section.contains("wger__get_routine - Fetch a routine.\n"));
        assert!(!section.contains("More."));
    }

    /// A single very long first line is capped on a char boundary, so a
    /// multi-byte character straddling the cap cannot split.
    #[test]
    fn build_deferred_section_caps_long_first_line_on_char_boundary() {
        let long: String = "é".repeat(DEFERRED_SUMMARY_MAX_CHARS + 50);
        let set = make_set(vec![make_stub("fs__read", &long)]);
        let section = build_deferred_tools_section(&set);
        let line = section
            .lines()
            .find(|l| l.starts_with("fs__read - "))
            .expect("tool line present");
        let summary = &line["fs__read - ".len()..];
        assert!(summary.ends_with("..."));
        assert_eq!(
            summary.trim_end_matches("...").chars().count(),
            DEFERRED_SUMMARY_MAX_CHARS
        );
    }

    /// The summary is a rendering concern only: keyword search still matches
    /// terms that appear solely in the body of a multi-line description.
    #[test]
    fn deferred_search_still_matches_description_body() {
        let set = make_set(vec![make_stub(
            "cronometer__get_diary",
            "Fetch the food diary for a day.\n\nReturns:\n    total_target_kcal for the day.",
        )]);
        let hits = set.search("target_kcal", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].prefixed_name, "cronometer__get_diary");
        assert!(!build_deferred_tools_section(&set).contains("target_kcal"));
    }

    #[test]
    fn build_deferred_section_includes_tool_search_instruction() {
        let stubs = vec![make_stub("fs__read_file", "Read a file")];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        let section = build_deferred_tools_section(&set);
        assert!(
            section.contains("tool_search"),
            "deferred section must instruct the LLM to use tool_search"
        );
        assert!(
            section.contains("## Deferred Tools"),
            "deferred section must include a heading"
        );
    }

    #[test]
    fn build_deferred_section_multiple_servers() {
        let stubs = vec![
            make_stub("server_a__list", "List items"),
            make_stub("server_a__create", "Create item"),
            make_stub("server_b__query", "Query records"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        let section = build_deferred_tools_section(&set);
        assert!(section.contains("server_a__list"));
        assert!(section.contains("server_a__create"));
        assert!(section.contains("server_b__query"));
        assert!(
            section.contains("tool_search"),
            "section must mention tool_search for multi-server setups"
        );
    }

    #[test]
    fn build_deferred_section_excluding_omits_named_stubs() {
        let stubs = vec![
            make_stub("fs__read_file", "Read a file"),
            make_stub("git__status", "Git status"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        let exclude: HashSet<String> = ["fs__read_file".to_string()].into_iter().collect();
        let section = build_deferred_tools_section_excluding(&set, None, &exclude);
        assert!(
            !section.contains("fs__read_file"),
            "pre-activated stub must not be advertised as deferred"
        );
        assert!(section.contains("git__status"));
    }

    #[test]
    fn build_deferred_section_excluding_all_returns_empty() {
        let stubs = vec![make_stub("fs__read_file", "Read a file")];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        let exclude: HashSet<String> = ["fs__read_file".to_string()].into_iter().collect();
        assert!(build_deferred_tools_section_excluding(&set, None, &exclude).is_empty());
    }

    #[test]
    fn keyword_search_ranks_by_hits() {
        let stubs = vec![
            make_stub("fs__read_file", "Read a file from disk"),
            make_stub("fs__write_file", "Write a file to disk"),
            make_stub("git__log", "Show git log"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };

        // "file read" should rank fs__read_file highest (2 hits vs 1)
        let results = set.search("file read", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].prefixed_name, "fs__read_file");
    }

    #[test]
    fn get_by_name_returns_correct_stub() {
        let stubs = vec![
            make_stub("a__one", "Tool one"),
            make_stub("b__two", "Tool two"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };
        assert!(set.get_by_name("a__one").is_some());
        assert!(set.get_by_name("nonexistent").is_none());
    }

    #[test]
    fn search_across_multiple_servers() {
        let stubs = vec![
            make_stub("server_a__read_file", "Read a file from disk"),
            make_stub("server_b__read_config", "Read configuration from database"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(McpRegistry::connect_all(&[]))
                    .unwrap(),
            ),
            security: test_security(),
        };

        // "read" should match stubs from both servers
        let results = set.search("read", 10);
        assert_eq!(results.len(), 2);

        // "file" should match only server_a
        let results = set.search("file", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].prefixed_name, "server_a__read_file");

        // "config database" should rank server_b highest (2 hits)
        let results = set.search("config database", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].prefixed_name, "server_b__read_config");
    }

    #[test]
    fn filter_by_policy_none_returns_unchanged() {
        // The centralized filter helper must be a no-op when no
        // policy is set (the common case for callers that do not
        // configure a `ToolAccessPolicy`).
        let stubs = vec![
            make_stub("fs__read_file", "Read a file"),
            make_stub("git__status", "Git status"),
        ];
        let set = DeferredMcpToolSet {
            stubs: stubs.clone(),
            registry: std::sync::Arc::new(empty_registry()),
            security: test_security(),
        };
        let filtered = set.filter_by_policy(None);
        assert_eq!(filtered.stubs.len(), stubs.len());
    }

    #[test]
    fn filter_by_policy_denied_removes_stubs() {
        // A policy that denies a tool by name must drop that stub
        // from the filtered set. The stub registry is irrelevant to
        // the policy decision (it is the named filter), so an empty
        // registry is fine.
        let stubs = vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ];
        let set = DeferredMcpToolSet {
            stubs,
            registry: std::sync::Arc::new(empty_registry()),
            security: test_security(),
        };
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };
        let filtered = set.filter_by_policy(Some(&policy));
        let names: Vec<&str> = filtered
            .stubs
            .iter()
            .map(|s| s.prefixed_name.as_str())
            .collect();
        assert!(names.contains(&"srv__visible"));
        assert!(!names.contains(&"srv__hidden"));
    }

    /// Build an empty [`McpRegistry`] for tests that exercise the
    /// stub set independently of any backing MCP server. The
    /// `filter_by_policy` helper does not consult the registry, so
    /// an empty one is sufficient.
    fn empty_registry() -> McpRegistry {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(McpRegistry::connect_all(&[]))
            .unwrap()
    }

    // ── Cerveau: bounded on-demand schemas ──

    struct NamedTool(String, usize);
    impl ::zeroclaw_api::attribution::Attributable for NamedTool {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Tool(::zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            <Self as Tool>::name(self)
        }
    }
    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            "named tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            // `self.1` pads the schema, to exercise the size bound.
            serde_json::json!({"type": "object", "description": "x".repeat(self.1)})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
        ) -> anyhow::Result<zeroclaw_api::tool::ToolResult> {
            unreachable!("not called in these tests")
        }
    }

    fn named(name: &str, pad: usize) -> Arc<dyn Tool> {
        Arc::new(NamedTool(name.to_string(), pad))
    }

    #[test]
    fn on_demand_schemas_past_the_count_bound_unload_least_recently_used() {
        let mut set = ActivatedToolSet::with_limits(2, usize::MAX);
        set.activate("pinned__always".into(), named("pinned__always", 0));
        set.activate_on_demand("srv__a".into(), named("srv__a", 0));
        set.activate_on_demand("srv__b".into(), named("srv__b", 0));
        // A call on `a` makes `b` the least recently used.
        set.note_used("srv__a");
        set.activate_on_demand("srv__c".into(), named("srv__c", 0));
        let unloaded = set.enforce_on_demand_limits(&HashSet::from(["srv__c".to_string()]));
        assert_eq!(unloaded, vec!["srv__b".to_string()]);
        assert!(set.get("srv__b").is_none());
        assert!(set.get("srv__a").is_some() && set.get("srv__c").is_some());
        // Pinned tools neither count nor unload.
        assert!(set.get("pinned__always").is_some());
        assert_eq!(set.tool_specs().len(), 3);
    }

    #[test]
    fn on_demand_schemas_past_the_size_bound_unload_but_never_the_current_call() {
        let mut set = ActivatedToolSet::with_limits(10, 1_000);
        set.activate_on_demand("srv__big".into(), named("srv__big", 700));
        set.activate_on_demand("srv__next".into(), named("srv__next", 700));
        let keep = HashSet::from(["srv__next".to_string()]);
        assert_eq!(
            set.enforce_on_demand_limits(&keep),
            vec!["srv__big".to_string()]
        );
        // Only kept names left: over the bound, but what was just asked for stays.
        set.activate_on_demand("srv__more".into(), named("srv__more", 700));
        let keep = HashSet::from(["srv__next".to_string(), "srv__more".to_string()]);
        assert!(set.enforce_on_demand_limits(&keep).is_empty());
        assert_eq!(set.on_demand_names().len(), 2);
    }

    #[test]
    fn an_unloaded_builtin_schema_is_hidden_again_and_can_be_selected_again() {
        let spec =
            |name: &str| ToolSpec::new(name, "builtin", serde_json::json!({"type": "object"}));
        let mut set = ActivatedToolSet::with_limits(1, usize::MAX);
        set.set_deferred_builtin_specs(vec![spec("cal"), spec("mail")]);
        set.select_builtin_schema("cal").unwrap();
        set.mark_loaded_on_demand("cal");
        set.select_builtin_schema("mail").unwrap();
        set.mark_loaded_on_demand("mail");
        let unloaded = set.enforce_on_demand_limits(&HashSet::from(["mail".to_string()]));
        assert_eq!(unloaded, vec!["cal".to_string()]);
        assert!(!set.is_builtin_schema_exposed("cal"));
        assert!(set.is_builtin_schema_exposed("mail"));
        assert!(set.select_builtin_schema("cal").is_some());
    }

    #[test]
    fn reloading_a_pinned_tool_keeps_it_pinned() {
        let mut set = ActivatedToolSet::with_limits(1, usize::MAX);
        set.activate("srv__pinned".into(), named("srv__pinned", 0));
        set.mark_loaded_on_demand("srv__pinned");
        assert!(set.on_demand_names().is_empty());
    }
}
