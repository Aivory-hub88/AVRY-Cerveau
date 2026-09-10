//! Non-fatal validation warnings for config that loads and validates
//! successfully (i.e. `Config::validate()` returns `Ok(())`) but needs user
//! attention. Warnings may identify a runtime inconsistency, an inert setting,
//! or a supported configuration that is being deprecated.

use serde::{Deserialize, Serialize};

/// One non-fatal validation issue surfaced after a successful save.
///
/// Stable codes (extend as new warnings are added):
/// - `memory_semantic_search_without_embedder`: `memory.search_mode` requests
///   vector search on sqlite memory, but no effective embedder is configured.
/// - `whatsapp_chat_policy_inert`: a WhatsApp Web `dm_policy` / `group_policy` /
///   `self_chat_mode` is set but the transport only consults them under
///   `mode = "personal"`, so it currently has no effect.
/// - `whatsapp_empty_group_allowlist_permits_all`: `allowed_groups` is empty in
///   a configuration where that list is the only group gate, so it permits every
///   group the linked account belongs to. Raised for `mode = "business"` (which
///   never consults `group_policy`) and for `mode = "personal"` with
///   `group_policy = "allowlist"`. Personal mode with `group_policy = "ignore"`
///   already drops every group message, and `group_policy = "all"` is an explicit
///   opt-in to open access, so neither is reported.
/// - `memory_config_knob_inert`: a `[memory]` knob is set to a non-default
///   value but has no runtime consumer yet, so it currently has no effect
///   (see `validate_memory_semantics` in `schema.rs` for the current list).
/// - `skills_prompt_injection_mode_full_deprecated`: explicit global full skill
///   injection remains supported but is deprecated before Schema V4.
/// - `peer_group_channel_dangling`: a `peer_groups.<name>.channel` dotted
///   alias (`<type>.<alias>`) does not resolve to any configured
///   `[channels.<type>.<alias>]` block — typically a typo that silently
///   authorizes nobody. `Config::validate()` already hard-errors this exact
///   condition (see `classify_peer_group_channel_ref` in `schema.rs`); this
///   warning is the non-fatal surface for callers (channel doctor,
///   config-load tracing) that need the diagnostic even when a config that
///   failed `validate()` is still allowed to boot.
/// - `context_compression_unsupported`: a `runtime_profiles.<alias>.context_compression`
///   knob (`enabled = true`, or any other field set to a non-default value)
///   has no runtime consumer — the context compressor was removed —
///   so it currently has no effect. One warning per non-default field (see
///   `collect_context_compression_ignored_warnings` in `schema.rs`).
/// - `delegation_depth_ceiling_unreachable`: `runtime_profiles.<alias>.max_delegation_depth`
///   is set above `1`, but both delegation modes (`Bounded` and `Independent`)
///   unconditionally strip the `delegate` tool from a sub-agent's own toolset
///   before it ever runs — a chain can reach depth 1 and no further, regardless
///   of the configured ceiling. Not raised for `0` (the documented "inherit the
///   default" sentinel) or `1` (matches the actual behavior exactly), only for a
///   value that represents a deliberate ask for a real multi-hop chain that
///   currently cannot happen (see `collect_delegation_depth_ceiling_warnings` in
///   `schema.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ValidationWarning {
    /// Stable machine-readable identifier for the warning class.
    pub code: String,
    /// Stable English fallback for logs and consumers without a localization catalog.
    /// User-facing surfaces should localize from `code` and use this only for unknown codes.
    pub message: String,
    /// Dotted property path the warning concerns
    /// (e.g. `"agents.researcher.model_provider"`).
    pub path: String,
}

impl ValidationWarning {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: path.into(),
        }
    }
}
