//! Heuristics for detecting tool-protocol fragments in streamed model text.

use std::collections::HashSet;
use zeroclaw_tool_call_parser::{
    ParsedToolCall, ToolProtocolEnvelopeKind, classify_tool_protocol_envelope,
    contains_tool_protocol_tag_call, looks_like_malformed_tool_protocol_envelope,
    looks_like_malformed_tool_protocol_envelope_for_known_tools, looks_like_tool_protocol_envelope,
    looks_like_tool_protocol_example, tool_protocol_envelope_mentions_known_tool,
};

pub(crate) fn longest_suffix_matching_prefix(text: &str, pattern: &str) -> usize {
    (1..pattern.len())
        .rev()
        .find(|&len| text.ends_with(&pattern[..len]))
        .unwrap_or(0)
}

pub(crate) fn find_embedded_protocol_candidate_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut earliest: Option<usize> = None;

    for pattern in [
        "<tool_call",
        "<toolcall",
        "<tool-call",
        "<invoke",
        "<function",
        "```tool",
        "```invoke",
        "```json",
    ] {
        if let Some(idx) = lower.find(pattern) {
            earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
        }
    }

    for key in ["\"tool_calls\"", "\"toolcalls\"", "\"function_call\""] {
        if let Some(key_idx) = lower.find(key)
            && let Some(json_start) = text[..key_idx].rfind(['{', '['])
        {
            earliest = Some(earliest.map_or(json_start, |current| current.min(json_start)));
        }
    }

    earliest
}

pub(crate) fn find_incomplete_protocol_candidate_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut earliest: Option<usize> = None;

    for pattern in [
        "<tool",
        "<invoke",
        "<function",
        "```tool",
        "```invoke",
        "```json",
    ] {
        if let Some(idx) = lower.rfind(pattern) {
            earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
        }
    }

    for delimiter in ['{', '['] {
        if let Some(idx) = text.rfind(delimiter) {
            let tail = &lower[idx..];
            if tail.contains("\"tool")
                || tail.contains("\"function")
                || tail.contains("\"call")
                || tail.len() <= 16
            {
                earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
            }
        }
    }

    earliest
}

pub(crate) fn starts_suspicious_protocol_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with('{')
        || lower.starts_with('[')
        || lower.starts_with("<tool")
        || lower.starts_with("<invoke")
        || lower.starts_with("<function")
        || lower.starts_with("```tool")
        || lower.starts_with("```invoke")
        || lower.starts_with("```json")
}

pub(crate) fn starts_suspicious_tag_or_fence_prefix(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("<tool")
        || lower.starts_with("<invoke")
        || lower.starts_with("<function")
        || lower.starts_with("```tool")
        || lower.starts_with("```invoke")
        || lower.starts_with("```json")
        || lower.starts_with("[tool_call]")
}

pub(crate) fn complete_non_protocol_json(text: &str, known_tool_names: &HashSet<String>) -> bool {
    let trimmed = text.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
        && (!looks_like_tool_protocol_envelope(trimmed)
            || !tool_protocol_envelope_mentions_known_tool(trimmed, known_tool_names))
}

pub(crate) fn complete_json_fence_protocol_state(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> Option<bool> {
    let trimmed = text.trim();
    let body = json_fence_body(trimmed)?;
    Some(
        looks_like_tool_protocol_envelope(body)
            && tool_protocol_envelope_mentions_known_tool(body, known_tool_names),
    )
}

pub(crate) fn detect_internal_protocol_without_tools(response: &str) -> Option<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_tool_protocol_example(trimmed) {
        return None;
    }

    (looks_like_malformed_tool_protocol_envelope(trimmed)
        || contains_tool_protocol_tag_call(trimmed)
        || classify_tool_protocol_envelope(trimmed)
            .is_some_and(|kind| matches!(kind, ToolProtocolEnvelopeKind::TaggedToolCall))
        || (classify_tool_protocol_envelope(trimmed).is_none()
            && looks_like_tool_protocol_envelope(trimmed)))
    .then(|| {
        "response resembled an internal tool protocol envelope but no tools were enabled".into()
    })
}

/// Shared envelope heuristic behind both [`detect_tool_call_parse_issue_for_known_tools`]
/// (no valid call parsed at all) and [`detect_residual_tool_protocol_issue`] (some valid
/// calls parsed, but leftover text still looks like a botched tool-call attempt).
fn detect_protocol_envelope_issue(trimmed: &str, known_tool_names: &HashSet<String>) -> Option<String> {
    if trimmed.is_empty() || looks_like_tool_protocol_example(trimmed) {
        return None;
    }

    let message = "response resembled an internal tool protocol envelope but no valid tool call could be parsed";

    if looks_like_malformed_tool_protocol_envelope_for_known_tools(trimmed, known_tool_names)
        || contains_tool_protocol_tag_call(trimmed)
    {
        return Some(message.into());
    }

    if let Some(kind) = classify_tool_protocol_envelope(trimmed) {
        return (matches!(
            kind,
            ToolProtocolEnvelopeKind::TaggedToolCall | ToolProtocolEnvelopeKind::ToolResult
        ) || tool_protocol_envelope_mentions_known_tool(trimmed, known_tool_names))
        .then(|| message.into());
    }

    looks_like_tool_protocol_envelope(trimmed).then(|| message.into())
}

pub(crate) fn detect_tool_call_parse_issue_for_known_tools(
    response: &str,
    parsed_calls: &[ParsedToolCall],
    known_tool_names: &HashSet<String>,
) -> Option<String> {
    if !parsed_calls.is_empty() {
        return None;
    }

    detect_protocol_envelope_issue(response.trim(), known_tool_names)
}

/// Detects a leftover, unparseable tool-call fragment for the *partial-parse* case: one or
/// more tool calls were already successfully extracted from this response, and
/// `residual_text` is whatever text `parse_tool_calls` left over after removing them (e.g. the
/// unconsumed tail after its tag loop bailed out of a second, malformed `<tool_call>` block).
///
/// This deliberately does NOT reuse [`detect_protocol_envelope_issue`]: that heuristic
/// classifies a *whole* zero-call response, and part of its logic (e.g.
/// `looks_like_malformed_tagged_tool_protocol_envelope`) requires re-parsing the text to come
/// back with no calls *and* no visible text — which is never true for a residual fragment,
/// since by construction it already failed to parse into a call and therefore reads back as
/// its own (non-empty) "visible text". Re-running that heuristic here would silently never
/// fire. Instead this checks the much narrower, purpose-built signal used elsewhere in this
/// module for streaming/incomplete fragments: does the residual still *start* with a
/// recognizable tool-call tag or fence marker (`<tool_call`, `<invoke`, ```` ```tool ````, …)?
///
/// A hit here never gates execution of the valid calls — it exists purely so the caller can
/// log/observe that part of the response was dropped, instead of the malformed fragment
/// silently leaking into the model's assistant history with nothing recorded about it.
pub(crate) fn detect_residual_tool_protocol_issue(residual_text: &str) -> Option<String> {
    let trimmed = residual_text.trim();
    if trimmed.is_empty() || looks_like_tool_protocol_example(trimmed) {
        return None;
    }

    starts_suspicious_tag_or_fence_prefix(trimmed).then(|| {
        "leftover fragment after a valid tool call still looks like an unparsed tool-call attempt"
            .to_string()
    })
}

pub(crate) fn json_fence_body(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let language = rest[..first_newline].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }

    let body_with_close = &rest[first_newline + 1..];
    let close_start = body_with_close.rfind("```")?;
    if !body_with_close[close_start + 3..].trim().is_empty() {
        return None;
    }
    Some(body_with_close[..close_start].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_tool_call_parser::parse_tool_calls;

    #[test]
    fn residual_issue_none_for_plain_leftover_narration() {
        // A fully clean response: one valid tool call, ordinary trailing prose.
        // Must never be flagged as a residual protocol issue (zero regression case).
        let response = r#"<tool_call>{"name": "shell", "arguments": {"command": "ls"}}</tool_call>
Done, let me know if you need anything else."#;
        let (residual, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(detect_residual_tool_protocol_issue(&residual), None);
    }

    #[test]
    fn residual_issue_detected_when_second_tool_call_is_malformed() {
        // Two `<tool_call>` blocks: the first parses cleanly, the second is missing
        // its closing tag (truncated mid-argument) — this is the "mixed valid +
        // malformed" case the fix targets. The valid call must survive in `calls`,
        // and the residual (which still starts with an open `<tool_call>` tag) must
        // be flagged for observability.
        let response = concat!(
            r#"<tool_call>{"name": "shell", "arguments": {"command": "ls"}}</tool_call>"#,
            "\n",
            r#"<tool_call>{"name": "shell", "arguments": {"command": "pwd"#
        );
        let (residual, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1, "the well-formed first tool call must survive");
        assert_eq!(calls[0].name, "shell");
        let issue = detect_residual_tool_protocol_issue(&residual);
        assert!(
            issue.is_some(),
            "leftover malformed tool-call fragment should be flagged, got residual={residual:?}"
        );
    }

    #[test]
    fn residual_issue_none_when_nothing_looks_like_a_tool_call() {
        let response = r#"<tool_call>{"name": "shell", "arguments": {"command": "ls"}}</tool_call>
By the way, brackets [1] and braces {like this} in prose aren't a tool call."#;
        let (residual, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(detect_residual_tool_protocol_issue(&residual), None);
    }
}
