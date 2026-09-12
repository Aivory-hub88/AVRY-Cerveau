//! Tool execution helpers extracted from `loop_`.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalManager;
use crate::observability::{Observer, ObserverEvent};
use crate::tools::{ActivatedToolSet, Tool};
use tokio::sync::mpsc::Sender;
use zeroclaw_api::agent::{ToolArtifact, TurnEvent};

// Items that still live in `loop_` — import via the parent module.
use super::loop_::{ParsedToolCall, ToolLoopCancelled, is_tool_loop_cancelled, scrub_credentials};
use super::turn::{ModelSwitchCallback, TurnMeta, scope_model_switch_state};

// ── Helpers ──────────────────────────────────────────────────────────────

/// If a just-completed tool call was a successful `TodoWrite`, build the
/// corresponding `TurnEvent::Plan` from its arguments. Returns `None`
/// for any other tool, a failed call, or arguments that fail to parse
/// (defensive — a real failure would already have `success == false`).
fn maybe_plan_event(
    call_name: &str,
    success: bool,
    call_arguments: &serde_json::Value,
) -> Option<zeroclaw_api::agent::TurnEvent> {
    if call_name != "TodoWrite" || !success {
        return None;
    }
    let entries = crate::tools::todo_write::parse_entries(call_arguments).ok()?;
    Some(zeroclaw_api::agent::TurnEvent::Plan { entries })
}

/// Look up a tool by name in a slice of boxed `dyn Tool` values.
pub fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

/// ADR-013 Phase 1 cheap gate: record a tool-call failure or a successful
/// `escalate_to_human` as a `skill_insights` row. This is the single
/// chokepoint every tool call in a live turn passes through
/// (`execute_one_tool`), so it doubles as the natural place to hook the
/// gate rather than special-casing `escalate_to_human` with a second
/// insertion point elsewhere.
///
/// Best-effort and fire-and-forget: absent ledger, absent tenant context
/// (host/internal turns have none), or a write failure are all silent
/// no-ops from the caller's point of view — this must never add latency or
/// a failure mode to the tool loop it's observing.
#[cfg(feature = "memory-postgres")]
fn maybe_record_skill_insight(call_name: &str, outcome: &Result<ToolExecutionOutcome>) {
    use zeroclaw_memory::skill_insight_ledger::InsightSource;

    let Ok(out) = outcome else { return };
    let source = if call_name == "escalate_to_human" && out.success {
        InsightSource::Escalation
    } else if !out.success {
        InsightSource::ToolFailure
    } else {
        return;
    };
    let Some(ledger) = zeroclaw_memory::skill_insight_ledger::current_skill_insight_ledger()
    else {
        return;
    };
    let Some(tenant) = crate::agent::tenant::current_tenant() else {
        return;
    };
    let session_id = super::tenant::current_turn_origin().and_then(|o| o.session_id.clone());
    let signal = match source {
        InsightSource::Escalation => "escalate_to_human invoked".to_string(),
        InsightSource::ToolFailure => format!(
            "tool '{call_name}' failed: {}",
            out.error_reason.as_deref().unwrap_or("(no reason given)")
        ),
    };
    let tenant_id = tenant.platform_user_id.clone();
    let agent_type = tenant.agent_type.clone();
    tokio::spawn(async move {
        if let Err(e) = ledger
            .create_insight(&tenant_id, &agent_type, session_id.as_deref(), source, &signal)
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "error": e.to_string() })),
                "skill insight write failed (non-fatal)"
            );
        }
    });
}

#[cfg(not(feature = "memory-postgres"))]
fn maybe_record_skill_insight(_call_name: &str, _outcome: &Result<ToolExecutionOutcome>) {}

// ── Hallucinated tool-name repair ───────────────────────────────────────
//
// Models occasionally emit a tool name that is close to, but not exactly,
// a registered one: a typo, `-` vs `_`, singular vs plural, or a stale
// alias. Rather than failing the call outright, `execute_one_tool` tries
// to repair the name by fuzzy-matching it against every tool actually
// available this turn (static registry + activated dynamic tools) before
// giving up. This is name-repair only — see `repair_unknown_tool_name`
// for the security note on why it can't widen what a turn is allowed to
// call.

/// Normalize a tool name for fuzzy comparison: lowercase, and collapse
/// `-`, `_`, and spaces to a single separator so `Some-Tool`, `some_tool`,
/// and `some tool` all compare as identical.
fn normalize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '-' | '_' | ' ' => '_',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// Iterative Levenshtein (edit) distance, operating on `char`s. Written by
/// hand rather than pulling in `strsim`/`edit-distance`: neither is a
/// direct dependency of this crate today (only a transitive one, via
/// `clap`), and one comparison at one call site doesn't earn a new direct
/// dependency.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Maximum edit distance (after normalization) still treated as "the same
/// tool, misspelled" rather than a genuinely different name.
const TOOL_NAME_REPAIR_MAX_DISTANCE: usize = 2;

/// Find the single closest match for `unknown` among `known` tool names.
///
/// Matching runs on the normalized form (see [`normalize_tool_name`]), so
/// separator/case-only differences count as distance 0. Otherwise plain
/// Levenshtein distance is used with a tolerance of
/// [`TOOL_NAME_REPAIR_MAX_DISTANCE`]. If two or more known names tie for
/// the closest match, the result is ambiguous and `None` is returned —
/// this never guesses between two live tools.
fn find_closest_tool_name(unknown: &str, known: &[&str]) -> Option<String> {
    let normalized_unknown = normalize_tool_name(unknown);

    let mut scored: Vec<(usize, &str)> = known
        .iter()
        .filter(|&&candidate| candidate != unknown)
        .map(|&candidate| {
            let normalized_candidate = normalize_tool_name(candidate);
            let distance = if normalized_candidate == normalized_unknown {
                0
            } else {
                levenshtein_distance(&normalized_unknown, &normalized_candidate)
            };
            (distance, candidate)
        })
        .filter(|&(distance, _)| distance <= TOOL_NAME_REPAIR_MAX_DISTANCE)
        .collect();

    scored.sort_by_key(|&(distance, _)| distance);

    match scored.as_slice() {
        [] => None,
        [(_, name)] => Some((*name).to_string()),
        [(d0, name0), (d1, _), ..] if d0 < d1 => Some((*name0).to_string()),
        _ => None, // Tie for the closest match — ambiguous, don't guess.
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolDispatchContext<'a> {
    pub tools_registry: &'a [Box<dyn Tool>],
    pub activated_tools: Option<&'a std::sync::Arc<std::sync::Mutex<ActivatedToolSet>>>,
    pub excluded_tools: &'a [String],
    pub model_switch_callback: Option<&'a ModelSwitchCallback>,
}

/// Try to repair an unknown tool name by fuzzy-matching it against every
/// tool name available this turn (static registry + activated dynamic
/// tools). Returns `None` when no candidate is close enough, or when the
/// closest match is ambiguous.
///
/// SECURITY NOTE: this only ever *selects among tools already registered
/// for this turn* — it does not create, authorize, or expose any tool
/// that wasn't already reachable. The caller still runs the ordinary
/// `is_excluded_tool` check against the *real* resolved tool's name
/// before executing (see the call site in `execute_one_tool`), exactly as
/// it would if the model had named that tool correctly to begin with.
/// Repair never bypasses that check and never touches approval/risk-tier
/// gating, which operates on the resolved tool the same way regardless of
/// how its name was determined.
fn repair_unknown_tool_name(call_name: &str, dispatch: ToolDispatchContext<'_>) -> Option<String> {
    let mut known: Vec<String> = dispatch
        .tools_registry
        .iter()
        .map(|t| t.name().to_string())
        .collect();

    if let Some(activated) = dispatch.activated_tools {
        let guard = match activated.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        known.extend(guard.tool_names().into_iter().map(str::to_string));
    }

    let known_refs: Vec<&str> = known.iter().map(String::as_str).collect();
    find_closest_tool_name(call_name, &known_refs)
}

fn is_excluded_tool(name: &str, excluded_tools: &[String]) -> bool {
    let name = name.trim();
    excluded_tools
        .iter()
        .any(|excluded| excluded.trim().eq_ignore_ascii_case(name))
}

fn unavailable_tool_outcome(
    call_name: &str,
    tool_call_id_owned: Option<String>,
    full_args: &str,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    duration: Duration,
) -> ToolExecutionOutcome {
    let reason = format!("Tool not available in this turn: {call_name}");
    observer.record_event(&ObserverEvent::ToolCall {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned,
        duration,
        success: false,
        arguments: Some(full_args.to_string()),
        result: Some(scrub_credentials(&reason)),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(|s| s.to_string()),
        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
        turn_id: Some(meta.turn_id.to_string()),
    });
    ToolExecutionOutcome {
        output: reason.clone(),
        success: false,
        error_reason: Some(reason),
        duration,
        receipt: None,
        output_data: None,
    }
}

// ── Outcome ──────────────────────────────────────────────────────────────

pub struct ToolExecutionOutcome {
    pub output: String,
    /// Structured output when the tool declared one (`ToolOutput::data`).
    /// Feeds SOP step capture and data-flow surfaces; the LLM sees only
    /// `output`.
    pub output_data: Option<serde_json::Value>,
    pub success: bool,
    /// Raw failure text on the data path. Credential scrubbing is a rendering
    /// concern applied at each human-facing surface (observer events,
    /// post-execution log line, CLI progress), never stored pre-scrubbed here.
    pub error_reason: Option<String>,
    pub duration: Duration,
    /// Cryptographic HMAC receipt proving this tool actually executed.
    /// Present only when tool receipts are enabled in config.
    pub receipt: Option<String>,
}

// ── Single tool execution ────────────────────────────────────────────────

pub(crate) async fn execute_one_tool(
    call_name: &str,
    call_arguments: serde_json::Value,
    tool_call_id: Option<&str>,
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<ToolExecutionOutcome> {
    let full_args = call_arguments.to_string();
    let tool_call_id_owned = tool_call_id.map(str::to_string);
    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned.clone(),
        arguments: Some(full_args.clone()),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(|s| s.to_string()),
        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
        turn_id: Some(meta.turn_id.to_string()),
    });
    let start = Instant::now();

    if is_excluded_tool(call_name, dispatch.excluded_tools) {
        return Ok(unavailable_tool_outcome(
            call_name,
            tool_call_id_owned,
            &full_args,
            meta,
            observer,
            start.elapsed(),
        ));
    }

    let static_tool = find_tool(dispatch.tools_registry, call_name);
    let activated_arc = if static_tool.is_none() {
        match dispatch.activated_tools {
            Some(at) => {
                let activated_tools = match at.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "tool": call_name,
                                "tool_call_id": tool_call_id,
                            })),
                            "activated-tool lock poisoned while resolving tool; recovering guard for read"
                        );
                        poisoned.into_inner()
                    }
                };
                activated_tools.get_resolved(call_name)
            }
            None => None,
        }
    } else {
        None
    };

    // Neither the static registry nor the activated dynamic tools have an
    // exact match. Try to repair a hallucinated name before failing — see
    // `repair_unknown_tool_name` for why this can't grant access beyond
    // what was already registered/authorized for this turn.
    let (static_tool, activated_arc, repaired_name) =
        if static_tool.is_none() && activated_arc.is_none() {
            match repair_unknown_tool_name(call_name, dispatch) {
                Some(repaired) => {
                    let repaired_static = find_tool(dispatch.tools_registry, &repaired);
                    let repaired_activated = if repaired_static.is_some() {
                        None
                    } else {
                        dispatch.activated_tools.and_then(|at| {
                            let guard = match at.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            guard.get_resolved(&repaired)
                        })
                    };
                    if repaired_static.is_some() || repaired_activated.is_some() {
                        (repaired_static, repaired_activated, Some(repaired))
                    } else {
                        (None, None, None)
                    }
                }
                None => (None, None, None),
            }
        } else {
            (static_tool, activated_arc, None)
        };

    if let Some(repaired) = &repaired_name {
        let note = format!("tool name auto-repaired: '{call_name}' -> '{repaired}'");
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_attrs(::serde_json::json!({
                    "requested_tool": call_name,
                    "resolved_tool": repaired,
                    "tool_call_id": tool_call_id,
                })),
            note.clone()
        );
        observer.record_event(&ObserverEvent::ToolCall {
            tool: "tool_name_repair".to_string(),
            tool_call_id: tool_call_id_owned.clone(),
            duration: Duration::from_secs(0),
            success: true,
            arguments: Some(full_args.clone()),
            result: Some(note),
            channel: Some(meta.channel_name.to_string()),
            agent_alias: meta.agent_alias.map(|s| s.to_string()),
            parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
            turn_id: Some(meta.turn_id.to_string()),
        });
    }

    let Some(tool) = static_tool.or(activated_arc.as_deref()) else {
        let reason = format!("Unknown tool: {call_name}");
        let duration = start.elapsed();
        observer.record_event(&ObserverEvent::ToolCall {
            tool: call_name.to_string(),
            tool_call_id: tool_call_id_owned.clone(),
            duration,
            success: false,
            arguments: Some(full_args.clone()),
            result: Some(scrub_credentials(&reason)),
            channel: Some(meta.channel_name.to_string()),
            agent_alias: meta.agent_alias.map(|s| s.to_string()),
            parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
            turn_id: Some(meta.turn_id.to_string()),
        });
        return Ok(ToolExecutionOutcome {
            output: reason.clone(),
            success: false,
            error_reason: Some(reason),
            duration,
            receipt: None,
            output_data: None,
        });
    };

    if is_excluded_tool(tool.name(), dispatch.excluded_tools) {
        return Ok(unavailable_tool_outcome(
            call_name,
            tool_call_id_owned,
            &full_args,
            meta,
            observer,
            start.elapsed(),
        ));
    }

    use ::zeroclaw_log::Instrument;
    let tool_span = ::zeroclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        tool = %call_name,
    );

    // Auto tool I/O propagation: emit Start with full input, run the
    // tool, then emit Complete or Fail with full output. Per-tool
    // execute() impls add zero logging.
    let _start_guard = tool_span.clone().entered();
    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Invoke)
            .with_category(::zeroclaw_log::EventCategory::Tool)
            .with_attrs(::serde_json::json!({
                "tool": call_name,
                "tool_call_id": tool_call_id,
                "input": call_arguments,
            })),
        format!("tool call: {call_name}")
    );
    drop(_start_guard);

    // Stable correlation id for this call's pending ToolCall and terminal
    // ToolResult. Native calls carry their own id; id-less text-protocol calls
    // get one synthesized UUID reused for both halves so ACP/WS clients key the
    // tool_call_update to the right pending tool_call.
    let event_call_id = tool_call_id_owned
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    if let Some(tx) = event_tx {
        let _ = tx
            .send(TurnEvent::ToolCall {
                id: event_call_id.clone(),
                name: call_name.to_string(),
                args: call_arguments.clone(),
            })
            .await;
    }

    let tool_future = tool
        .execute(call_arguments.clone())
        .instrument(tool_span.clone());
    let execute = async {
        if let Some(token) = cancellation_token {
            tokio::select! {
                () = token.cancelled() => Err::<_, anyhow::Error>(ToolLoopCancelled.into()),
                result = tool_future => Ok(result),
            }
        } else {
            Ok(tool_future.await)
        }
    };
    let tool_result = if let Some(model_switch_callback) = dispatch.model_switch_callback {
        scope_model_switch_state(Arc::clone(model_switch_callback), execute).await
    } else {
        execute.await
    }?;

    let outcome = {
        let _result_guard = tool_span.entered();
        match tool_result {
            Ok(r) => {
                let duration = start.elapsed();
                if r.success {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(
                            module_path!(),
                            ::zeroclaw_log::Action::Complete
                        )
                        .with_category(::zeroclaw_log::EventCategory::Tool)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success)
                        .with_duration(duration.as_millis() as u64)
                        .with_attrs(::serde_json::json!({
                            "tool": call_name,
                            "tool_call_id": tool_call_id,
                            "input": call_arguments,
                            "output": r.output,
                        })),
                        format!("tool result: {call_name}")
                    );
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_duration(duration.as_millis() as u64)
                            .with_attrs(::serde_json::json!({
                                "tool": call_name,
                                "tool_call_id": tool_call_id,
                                "input": call_arguments,
                                "error": r.error.clone().unwrap_or_default(),
                                "output": r.output,
                            })),
                        format!("tool failed: {call_name}")
                    );
                }
                if r.success {
                    let normalized_output = if r.output.is_empty() {
                        "(no output)"
                    } else {
                        &r.output
                    };
                    let receipt = receipt_generator.map(|receipt_gen| {
                        receipt_gen.generate_now(call_name, &call_arguments, normalized_output)
                    });
                    observer.record_event(&ObserverEvent::ToolCall {
                        tool: call_name.to_string(),
                        tool_call_id: tool_call_id_owned.clone(),
                        duration,
                        success: true,
                        arguments: Some(full_args.clone()),
                        result: Some(scrub_credentials(normalized_output)),
                        channel: Some(meta.channel_name.to_string()),
                        agent_alias: meta.agent_alias.map(|s| s.to_string()),
                        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                        turn_id: Some(meta.turn_id.to_string()),
                    });
                    Ok(ToolExecutionOutcome {
                        output: normalized_output.to_string(),
                        output_data: r.output.into_data(),
                        success: true,
                        error_reason: None,
                        duration,
                        receipt,
                    })
                } else {
                    let reason = r.error.unwrap_or_else(|| r.output.into_string());
                    observer.record_event(&ObserverEvent::ToolCall {
                        tool: call_name.to_string(),
                        tool_call_id: tool_call_id_owned.clone(),
                        duration,
                        success: false,
                        arguments: Some(full_args.clone()),
                        result: Some(scrub_credentials(&reason)),
                        channel: Some(meta.channel_name.to_string()),
                        agent_alias: meta.agent_alias.map(|s| s.to_string()),
                        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                        turn_id: Some(meta.turn_id.to_string()),
                    });
                    Ok(ToolExecutionOutcome {
                        output: format!("Error: {reason}"),
                        success: false,
                        error_reason: Some(reason),
                        duration,
                        receipt: None,
                        output_data: None,
                    })
                }
            }
            Err(e) => {
                let duration = start.elapsed();
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Tool)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_duration(duration.as_millis() as u64)
                        .with_attrs(::serde_json::json!({
                            "tool": call_name,
                            "tool_call_id": tool_call_id,
                            "input": call_arguments,
                            "error": format!("{e:?}"),
                        })),
                    format!("tool error: {call_name}")
                );
                let reason = format!("Error executing {call_name}: {e}");
                observer.record_event(&ObserverEvent::ToolCall {
                    tool: call_name.to_string(),
                    tool_call_id: tool_call_id_owned.clone(),
                    duration,
                    success: false,
                    arguments: Some(full_args.clone()),
                    result: Some(scrub_credentials(&reason)),
                    channel: Some(meta.channel_name.to_string()),
                    agent_alias: meta.agent_alias.map(|s| s.to_string()),
                    parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                    turn_id: Some(meta.turn_id.to_string()),
                });
                Ok(ToolExecutionOutcome {
                    output: reason.clone(),
                    success: false,
                    error_reason: Some(reason),
                    duration,
                    receipt: None,
                    output_data: None,
                })
            }
        }
    };

    maybe_record_skill_insight(call_name, &outcome);

    if let Some(tx) = event_tx
        && let Ok(out) = &outcome
    {
        let _ = tx
            .send(TurnEvent::ToolResult {
                id: event_call_id.clone(),
                name: call_name.to_string(),
                output: scrub_credentials(&out.output),
                artifact: out
                    .output_data
                    .as_ref()
                    .and_then(ToolArtifact::from_delivered_data),
            })
            .await;
    }

    // After the ToolResult card closes, publish the plan if this was a
    // successful TodoWrite. Whole-list replace; parse failures are
    // swallowed (the ToolResult already conveyed success/failure).
    if let Some(tx) = event_tx
        && let Ok(out) = &outcome
        && let Some(plan_event) = maybe_plan_event(call_name, out.success, &call_arguments)
    {
        let _ = tx.send(plan_event).await;
    }

    outcome
}

// ── Parallel / sequential decision ───────────────────────────────────────

/// Argument keys that, across the tool registry, carry a filesystem path
/// (see `file_write`, `file_edit`, `file_download`, `file_upload`). Kept as
/// a flat list rather than per-tool metadata — this is a conservative,
/// best-effort heuristic, not a full dependency analysis.
const PATH_ARG_KEYS: &[&str] = &["path", "file_path", "dest_path"];

/// Extract every path-shaped string argument from a single tool call.
/// Missing keys or non-string values are simply skipped — a tool call with
/// no path arguments contributes nothing to the overlap check.
fn path_args_of(call: &ParsedToolCall) -> Vec<&str> {
    let Some(obj) = call.arguments.as_object() else {
        return Vec::new();
    };
    PATH_ARG_KEYS
        .iter()
        .filter_map(|key| obj.get(*key).and_then(|v| v.as_str()))
        .collect()
}

/// Conservative check: does this batch contain two or more tool calls that
/// reference the same path argument value (plain string comparison, no
/// canonicalization)? If so, they may race on the same file/resource and
/// must not be dispatched concurrently.
///
/// This mirrors Hermes Agent's `_plan_tool_batch_segments`, scaled down to
/// this codebase's existing "heuristic, not a dependency graph" style.
fn batch_has_path_overlap(tool_calls: &[ParsedToolCall]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for call in tool_calls {
        for path in path_args_of(call) {
            if seen.contains(&path) {
                return true;
            }
            seen.push(path);
        }
    }
    false
}

pub fn should_execute_tools_in_parallel(
    tool_calls: &[ParsedToolCall],
    approval: Option<&ApprovalManager>,
) -> bool {
    if tool_calls.len() <= 1 {
        return false;
    }

    // tool_search activates deferred MCP tools into ActivatedToolSet.
    // Running tool_search in parallel with the tools it activates causes a
    // race condition where the tool lookup happens before activation completes.
    // Force sequential execution whenever tool_search is in the batch.
    if tool_calls.iter().any(|call| call.name == "tool_search") {
        return false;
    }

    if let Some(mgr) = approval
        && tool_calls.iter().any(|call| mgr.needs_approval(&call.name))
    {
        // Approval-gated calls must keep sequential handling so the caller can
        // enforce CLI prompt/deny policy consistently.
        return false;
    }

    // Two or more calls touching the same path/file argument race on that
    // resource if dispatched concurrently (e.g. two `file_write` calls to
    // the same path). Fall back to sequential rather than risk an
    // unpredictable interleaving.
    if batch_has_path_overlap(tool_calls) {
        return false;
    }

    true
}

// ── Parallel execution ───────────────────────────────────────────────────

pub(crate) async fn execute_tools_parallel(
    tool_calls: &[ParsedToolCall],
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<Vec<Option<ToolExecutionOutcome>>> {
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|call| {
            execute_one_tool(
                &call.name,
                call.arguments.clone(),
                call.tool_call_id.as_deref(),
                dispatch,
                meta,
                observer,
                cancellation_token,
                receipt_generator,
                event_tx,
            )
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;
    let mut slots = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(outcome) => slots.push(Some(outcome)),
            Err(e) if is_tool_loop_cancelled(&e) => slots.push(None),
            Err(e) => return Err(e),
        }
    }
    Ok(slots)
}

// ── Sequential execution ─────────────────────────────────────────────────

pub(crate) async fn execute_tools_sequential(
    tool_calls: &[ParsedToolCall],
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<Vec<Option<ToolExecutionOutcome>>> {
    let mut slots: Vec<Option<ToolExecutionOutcome>> = Vec::with_capacity(tool_calls.len());

    for call in tool_calls {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            break;
        }
        let outcome = match execute_one_tool(
            &call.name,
            call.arguments.clone(),
            call.tool_call_id.as_deref(),
            dispatch,
            meta,
            observer,
            cancellation_token,
            receipt_generator,
            event_tx,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) if is_tool_loop_cancelled(&e) => break,
            Err(e) => return Err(e),
        };
        slots.push(Some(outcome));
    }

    slots.resize_with(tool_calls.len(), || None);
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::{ToolDispatchContext, execute_one_tool};
    use crate::observability::noop::NoopObserver;
    use crate::tools::ActivatedToolSet;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use zeroclaw_api::tool::Tool;

    /// Minimal tool that records invocations. Used to verify that the
    /// poisoned-lock recovery path still resolves an activated tool and
    /// calls its execute method successfully.
    struct CountingTool {
        name: String,
        invocations: Arc<AtomicUsize>,
    }

    impl CountingTool {
        fn new(name: &str, invocations: Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                invocations,
            }
        }
    }

    impl zeroclaw_api::attribution::Attributable for CountingTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::System
        }
        fn alias(&self) -> &str {
            "test-counting-tool"
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Counts executions for poisoned-lock tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "executed via poisoned lock recovery".into(),
                error: None,
            })
        }
    }

    // ── Tool-name repair (fuzzy match) tests ─────────────────────────────

    use super::find_closest_tool_name;

    #[test]
    fn find_closest_tool_name_repairs_single_character_typo() {
        // "file_readef" is one substitution away from "file_reader"
        // (edit distance 1) and much farther from every other candidate,
        // so this must resolve unambiguously.
        let known = ["file_reader", "file_writer", "shell"];
        assert_eq!(
            find_closest_tool_name("file_readef", &known),
            Some("file_reader".to_string())
        );
    }

    #[test]
    fn find_closest_tool_name_returns_none_for_distant_name() {
        // Nothing in `known` is within the repair tolerance of this name,
        // so behavior must stay "fail like before" — no guessing.
        let known = ["file_reader", "file_writer", "shell"];
        assert_eq!(
            find_closest_tool_name("completely_unrelated_tool_xyz", &known),
            None
        );
    }

    #[test]
    fn find_closest_tool_name_returns_none_when_two_candidates_tie() {
        // "cat" is edit distance 1 from both "car" and "bat" — a genuine
        // tie for closest match. Auto-repair must refuse to guess between
        // two live tools and behave as if nothing matched.
        let known = ["car", "bat"];
        assert_eq!(find_closest_tool_name("cat", &known), None);
    }

    #[tokio::test]
    async fn execute_one_tool_repairs_hallucinated_typo_to_registered_static_tool() {
        // A model that hallucinates "file_readef" instead of the
        // registered "file_reader" should still get its call executed
        // against the real tool, with an audit trail recorded via the
        // existing ObserverEvent mechanism (asserted indirectly here by
        // checking the call actually ran).
        let invocations = Arc::new(AtomicUsize::new(0));
        let tool: Box<dyn Tool> = Box::new(CountingTool::new(
            "file_reader",
            Arc::clone(&invocations),
        ));
        let registry = vec![tool];

        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };

        let outcome = execute_one_tool(
            "file_readef",
            serde_json::json!({}),
            None,
            ToolDispatchContext {
                tools_registry: &registry,
                activated_tools: None,
                excluded_tools: &[],
                model_switch_callback: None,
            },
            &meta,
            &NoopObserver,
            None,
            None,
            None,
        )
        .await
        .expect("hallucinated typo should be repaired and dispatched");

        assert!(
            outcome.success,
            "repaired call should execute the real tool successfully"
        );
        assert!(
            outcome.output.contains("executed via poisoned lock recovery"),
            "output should come from the repaired tool's execute()"
        );
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            1,
            "the repaired tool should have been invoked exactly once"
        );
    }

    #[tokio::test]
    async fn execute_one_tool_recovers_poisoned_activated_tool_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let invocations = Arc::new(AtomicUsize::new(0));
        let activated_tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
            "docker-mcp__extract_text",
            Arc::clone(&invocations),
        ));
        activated
            .lock()
            .unwrap()
            .activate("docker-mcp__extract_text".into(), activated_tool);

        // Poison the mutex by panicking while holding the lock in a
        // separate thread.
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("deliberately poison the activated-tools lock");
        })
        .join();

        // execute_one_tool must recover the poisoned lock and resolve
        // the activated tool without panicking.
        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let outcome = execute_one_tool(
            "docker-mcp__extract_text",
            serde_json::json!({}),
            None,
            ToolDispatchContext {
                tools_registry: &[], // no static tools - force activated-tools path
                activated_tools: Some(&activated),
                excluded_tools: &[],
                model_switch_callback: None,
            },
            &meta,
            &NoopObserver,
            None,
            None,
            None,
        )
        .await
        .expect("execute_one_tool should recover from poisoned lock");

        assert!(
            outcome.success,
            "activated tool execution should succeed after poisoned lock recovery"
        );
        assert!(
            outcome
                .output
                .contains("executed via poisoned lock recovery"),
            "tool output should come from the recovered activated tool"
        );
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            1,
            "recovered activated tool should have been invoked exactly once"
        );
    }

    #[tokio::test]
    async fn execute_one_tool_blocks_excluded_activated_suffix_resolution() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let invocations = Arc::new(AtomicUsize::new(0));
        let activated_tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
            "docker-mcp__extract_text",
            Arc::clone(&invocations),
        ));
        activated
            .lock()
            .unwrap()
            .activate("docker-mcp__extract_text".into(), activated_tool);

        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let excluded = vec!["docker-mcp__extract_text".to_string()];
        let outcome = execute_one_tool(
            "extract_text",
            serde_json::json!({}),
            Some("call-1"),
            ToolDispatchContext {
                tools_registry: &[],
                activated_tools: Some(&activated),
                excluded_tools: &excluded,
                model_switch_callback: None,
            },
            &meta,
            &NoopObserver,
            None,
            None,
            None,
        )
        .await
        .expect("excluded activated tool should return an unavailable outcome");

        assert!(!outcome.success);
        assert_eq!(
            outcome.output,
            "Tool not available in this turn: extract_text"
        );
        assert_eq!(invocations.load(Ordering::SeqCst), 0);
    }

    use super::should_execute_tools_in_parallel;
    use crate::agent::loop_::ParsedToolCall;
    use crate::approval::ApprovalManager;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::schema::RiskProfileConfig;

    fn parsed_tool_call(name: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: name.to_string(),
            arguments: serde_json::json!({}),
            tool_call_id: None,
            arguments_parse_error: None,
        }
    }

    fn parsed_tool_call_with_args(name: &str, arguments: serde_json::Value) -> ParsedToolCall {
        ParsedToolCall {
            name: name.to_string(),
            arguments,
            tool_call_id: None,
            arguments_parse_error: None,
        }
    }

    fn supervised_risk_profile() -> RiskProfileConfig {
        RiskProfileConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into()],
            always_ask: vec!["shell".into()],
            ..RiskProfileConfig::default()
        }
    }

    // --- tool_search branch---

    #[test]
    fn tool_search_in_batch_forces_serial() {
        // Two non-approval-gated tools in a batch where one is `tool_search`
        // must run sequentially. Without the `tool_search` branch the default
        // path would return `true` and the runtime would dispatch them in
        // parallel, racing the lookup against the activation.
        let calls = vec![
            parsed_tool_call("tool_search"),
            parsed_tool_call("file_read"),
        ];

        assert!(
            !should_execute_tools_in_parallel(&calls, None),
            "batch containing tool_search must force sequential execution (line 349-351)"
        );
    }

    #[test]
    fn tool_search_with_approval_required_in_batch_still_forces_serial() {
        // When both branches would trigger, the test only needs to confirm
        // the call still returns `false` — the ordering between the
        // `tool_search` branch and the approval branch is an implementation
        // detail. The important invariant is: `tool_search` present ⇒ serial.
        let calls = vec![parsed_tool_call("tool_search"), parsed_tool_call("shell")];
        let approval_cfg = zeroclaw_config::schema::RiskProfileConfig::default();
        let approval_mgr = ApprovalManager::from_risk_profile(&approval_cfg);

        assert!(
            !should_execute_tools_in_parallel(&calls, Some(&approval_mgr)),
            "tool_search in a mixed approval batch must still force sequential execution"
        );
    }

    #[test]
    fn non_search_non_approval_batch_remains_parallel_eligible() {
        let calls = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("memory_recall"),
        ];

        assert!(
            should_execute_tools_in_parallel(&calls, None),
            "non-tool_search, non-approval batch must remain parallel-eligible (default branch)"
        );
    }

    // --- approval-required + control branches---

    #[test]
    fn approval_required_batch_forces_sequential() {
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("shell"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "batch with approval-required tool must execute sequentially"
        );
    }

    #[test]
    fn approval_required_alone_in_batch_still_sequential() {
        // A two-element batch where one tool requires approval must still
        // take the serial branch (length check above already returns false
        // for len <= 1; this asserts the approval branch is the actual gate).
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![parsed_tool_call("file_read"), parsed_tool_call("shell")];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "approval branch must trigger regardless of approval tool position"
        );
    }

    #[test]
    fn mixed_batch_with_approval_forces_serial_even_with_parallel_candidates() {
        // Mixed batch: two file_read (parallel candidates) plus one shell
        // (approval-required). The presence of `shell` must force serial
        // execution, even though the other two could otherwise run in
        // parallel.
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("shell"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "mixed batch must serialize when any approval-required tool is present"
        );
    }

    #[test]
    fn parallel_when_no_approval_and_no_tool_search() {
        // Control case: a batch of three non-approval, non-tool_search
        // calls under `Supervised` (where `file_read` is auto-approved and
        // `shell` is approval-required) may run in parallel.
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("file_read"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "non-approval, non-tool_search batch must run in parallel when allowed"
        );
    }

    #[test]
    fn full_autonomy_batch_with_unknown_tool_runs_in_parallel() {
        // Under `Full` autonomy, no tool requires approval — `needs_approval`
        // returns false for every name. The control case extends to a batch
        // whose names would otherwise be unknown to supervised profile.
        let full = RiskProfileConfig {
            level: AutonomyLevel::Full,
            ..RiskProfileConfig::default()
        };
        let mgr = ApprovalManager::for_non_interactive(&full);
        let batch = vec![
            parsed_tool_call("file_write"),
            parsed_tool_call("shell"),
            parsed_tool_call("anything"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "full autonomy never prompts, so parallel execution is allowed"
        );
    }

    #[test]
    fn no_approval_manager_with_multi_call_batch_runs_in_parallel() {
        // When the caller passes `None` for `approval` and no tool in the
        // batch is `tool_search`, the function takes the parallel branch
        // unconditionally — useful for the tests / harnesses that exercise
        // the tool loop without an approval manager.
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("memory_recall"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, None),
            "no approval manager + non-tool_search batch must run in parallel"
        );
    }

    // --- path-overlap branch ---

    #[test]
    fn overlapping_path_args_force_serial() {
        // Two `file_write` calls targeting the same path race on that file
        // if dispatched concurrently. The overlap check must force serial
        // execution even though neither call is `tool_search` nor
        // approval-gated.
        let batch = vec![
            parsed_tool_call_with_args("file_write", serde_json::json!({"path": "notes.txt", "content": "a"})),
            parsed_tool_call_with_args("file_write", serde_json::json!({"path": "notes.txt", "content": "b"})),
        ];

        assert!(
            !should_execute_tools_in_parallel(&batch, None),
            "batch with two tool calls writing the same path must force sequential execution"
        );
    }

    #[test]
    fn distinct_path_args_remain_parallel_eligible() {
        // Regression guard: different paths must not trip the overlap
        // heuristic and must remain parallel-eligible as before.
        let batch = vec![
            parsed_tool_call_with_args("file_write", serde_json::json!({"path": "a.txt", "content": "a"})),
            parsed_tool_call_with_args("file_write", serde_json::json!({"path": "b.txt", "content": "b"})),
        ];

        assert!(
            should_execute_tools_in_parallel(&batch, None),
            "batch with distinct path arguments must remain parallel-eligible"
        );
    }

    #[test]
    fn tool_call_without_path_args_does_not_panic_and_has_no_overlap() {
        // A tool call whose arguments carry no path-shaped field (or no
        // object at all) must be treated as contributing no path to the
        // overlap check, and must never cause a panic.
        let batch = vec![
            parsed_tool_call("calculator"),
            parsed_tool_call_with_args("file_write", serde_json::json!({"path": "a.txt", "content": "a"})),
            parsed_tool_call_with_args("memory_recall", serde_json::json!("not-an-object")),
        ];

        assert!(
            should_execute_tools_in_parallel(&batch, None),
            "tool calls lacking path arguments must not trigger a false-positive overlap"
        );
    }

    // ── Plan emission tests ────────────────────────────────────────────────

    #[cfg(test)]
    mod plan_emission_tests {
        use super::super::maybe_plan_event;
        use serde_json::json;

        #[test]
        fn plan_event_built_for_successful_todowrite() {
            let args = json!({ "todos": [ { "content": "A", "status": "pending" } ] });
            let ev = maybe_plan_event("TodoWrite", true, &args);
            match ev {
                Some(zeroclaw_api::agent::TurnEvent::Plan { entries }) => {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].content, "A");
                }
                _ => panic!("expected a Plan event"),
            }
        }

        #[test]
        fn no_plan_event_for_other_tools() {
            let args = json!({ "todos": [ { "content": "A", "status": "pending" } ] });
            assert!(maybe_plan_event("shell", true, &args).is_none());
        }

        #[test]
        fn no_plan_event_for_failed_todowrite() {
            let args = json!({ "todos": [ { "content": "A", "status": "pending" } ] });
            assert!(maybe_plan_event("TodoWrite", false, &args).is_none());
        }

        #[test]
        fn no_plan_event_for_unparseable_todowrite_args() {
            let args = json!({ "todos": [ { "status": "pending" } ] });
            assert!(maybe_plan_event("TodoWrite", true, &args).is_none());
        }

        #[test]
        fn empty_list_produces_clear_plan_event() {
            let args = json!({ "todos": [] });
            match maybe_plan_event("TodoWrite", true, &args) {
                Some(zeroclaw_api::agent::TurnEvent::Plan { entries }) => {
                    assert!(entries.is_empty());
                }
                _ => panic!("expected an empty Plan event (clear)"),
            }
        }
    }
}
