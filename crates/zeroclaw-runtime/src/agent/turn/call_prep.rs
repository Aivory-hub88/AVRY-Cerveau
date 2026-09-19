//! The per-call preparation loop: `before_tool_call` hook, delivery defaults,
//! the approval gate, the duplicate-call gate, and start logging — producing
//! the executable subset of this round's tool calls.

use super::approval_gate::{ApprovalGateOutcome, gate_tool_approval};
use super::context::TurnCtx;
use super::delivery_defaults::maybe_inject_channel_delivery_defaults;
use super::events::{StreamDelta, emit_tool_call_pair};
use super::redact::scrub_credentials;
use crate::agent::tool_execution::ToolExecutionOutcome;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::collections::HashSet;
use std::time::Duration;
use zeroclaw_tool_call_parser::{ParsedToolCall, canonicalize_json_for_tool_signature};

pub(crate) struct PreparedToolCalls {
    pub(crate) ordered_results: Vec<Option<(String, Option<String>, ToolExecutionOutcome)>>,
    pub(crate) executable_indices: Vec<usize>,
    pub(crate) executable_calls: Vec<ParsedToolCall>,
    /// Cerveau (enterprise-hardening round 1, F-2): one slot per
    /// `executable_calls` entry — `Some(key)` when that call claimed an F-2
    /// idempotency key and post-exec must `complete`/`release` it,
    /// `None` for a `Safe`-tier call or a manager with no ledger
    /// configured (today's unchanged behavior).
    pub(crate) claimed_idem_keys: Vec<Option<String>>,
}

fn tool_call_signature(tool_name: &str, tool_args: &serde_json::Value) -> (String, String) {
    let canonical_args = canonicalize_json_for_tool_signature(tool_args);
    let args_json = serde_json::to_string(&canonical_args).unwrap_or_else(|_| "{}".to_string());
    (tool_name.trim().to_ascii_lowercase(), args_json)
}

async fn record_duplicate_tool_call(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    iteration: usize,
) -> ToolExecutionOutcome {
    let duplicate =
        format!("Skipped duplicate tool call '{tool_name}' with identical arguments in this turn.");
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Skip)
            .with_category(::zeroclaw_log::EventCategory::Tool)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "model": ctx.model,
                "iteration": iteration + 1,
                "tool": tool_name,
                "arguments": scrub_credentials(&tool_args.to_string()),
                "result": duplicate,
                "deduplicated": true,
                "trace_id": ctx.turn_id,
            })),
        "tool_call_result"
    );
    if let Some(tx) = ctx.on_delta {
        let _ = tx
            .send(StreamDelta::Status(format!(
                "\u{274c} {}: {}\n",
                tool_name, duplicate
            )))
            .await;
    }
    ToolExecutionOutcome {
        output: duplicate.clone(),
        success: false,
        error_reason: Some(duplicate),
        duration: Duration::ZERO,
        receipt: None,
        output_data: None,
    }
}

/// Run per-call preparation over this round's parsed tool calls (upstream
/// loop body, per-call prep loop).
pub(crate) async fn prepare_tool_calls(
    ctx: &TurnCtx<'_>,
    tool_calls: &[ParsedToolCall],
    seen_tool_signatures: &mut HashSet<(String, String)>,
    prompt_approval_tool_signatures: &mut HashSet<(String, String)>,
    iteration: usize,
    dedup_enabled: bool,
) -> Result<PreparedToolCalls> {
    let mut ordered_results: Vec<Option<(String, Option<String>, ToolExecutionOutcome)>> =
        (0..tool_calls.len()).map(|_| None).collect();
    let mut executable_indices: Vec<usize> = Vec::new();
    let mut executable_calls: Vec<ParsedToolCall> = Vec::new();
    let mut claimed_idem_keys: Vec<Option<String>> = Vec::new();
    let mut prompt_approval_tool_signatures_this_round: HashSet<(String, String)> = HashSet::new();

    for (idx, call) in tool_calls.iter().enumerate() {
        // ── Reject calls whose arguments failed to parse as JSON ────────
        // The model emitted a tool call, but its `arguments` payload was not
        // valid JSON. Silently substituting `{}` and running the tool anyway
        // would execute it with arguments nobody actually sent — dangerous
        // for any mutating tool (file write, delegate, etc). Report the
        // failure back to the model instead, without ever entering the
        // hook/approval/execution pipeline for this call.
        if let Some(parse_error) = &call.arguments_parse_error {
            let message = format!(
                "Tool call '{}' was not executed: {parse_error}",
                call.name
            );
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": ctx.model,
                        "iteration": iteration + 1,
                        "tool": call.name,
                        "result": message,
                        "trace_id": ctx.turn_id,
                    })),
                "tool_call_result"
            );
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{274c} {}: {}\n",
                        call.name, message
                    )))
                    .await;
            }
            let outcome = ToolExecutionOutcome {
                output: message.clone(),
                success: false,
                error_reason: Some(message),
                duration: Duration::ZERO,
                receipt: None,
                output_data: None,
            };
            if let Some(tx) = ctx.event_tx {
                emit_tool_call_pair(tx, call, &outcome).await;
            }
            ordered_results[idx] = Some((call.name.clone(), call.tool_call_id.clone(), outcome));
            continue;
        }

        // ── Circuit breaker: the tool's SERVER has been failing ─────────
        // After N consecutive server-side failures (across turns) for this
        // tenant, answer with "temporarily unavailable, do not retry" instead
        // of calling a dead server again. Never enters hooks/approval/execution.
        if let Some(outcome) = super::tool_breaker::check_open(ctx, &call.name) {
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{274c} {}: {}\n",
                        call.name, outcome.output
                    )))
                    .await;
            }
            if let Some(tx) = ctx.event_tx {
                emit_tool_call_pair(tx, call, &outcome).await;
            }
            ordered_results[idx] = Some((call.name.clone(), call.tool_call_id.clone(), outcome));
            continue;
        }

        // ── Hook: before_tool_call (modifying) ──────────
        let mut tool_name = call.name.clone();
        let mut tool_args = call.arguments.clone();
        if let Some(hooks) = ctx.hooks {
            match hooks
                .run_before_tool_call(tool_name.clone(), tool_args.clone())
                .await
            {
                crate::hooks::HookResult::Cancel(reason) => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel).with_category(::zeroclaw_log::EventCategory::Tool).with_attrs(::serde_json::json!({"tool": call.name, "reason": reason.to_string()})), "tool call cancelled by hook");
                    let cancelled = format!("Cancelled by hook: {reason}");
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "model": ctx.model,
                                "iteration": iteration + 1,
                                "tool": call.name,
                                "arguments": scrub_credentials(&tool_args.to_string()),
                                "result": cancelled,
                                "trace_id": ctx.turn_id,
                            })),
                        "tool_call_result"
                    );
                    if let Some(tx) = ctx.on_delta {
                        let _ = tx
                            .send(StreamDelta::Status(format!(
                                "\u{274c} {}: {}\n",
                                call.name,
                                truncate_with_ellipsis(&scrub_credentials(&cancelled), 200)
                            )))
                            .await;
                    }
                    let outcome = ToolExecutionOutcome {
                        output: cancelled,
                        success: false,
                        error_reason: Some(reason),
                        duration: Duration::ZERO,
                        receipt: None,
                        output_data: None,
                    };
                    // Streaming consumers still see the call and its
                    // hook-cancel outcome as a ToolCall/ToolResult pair,
                    // as the direct execution path always emitted.
                    if let Some(tx) = ctx.event_tx {
                        emit_tool_call_pair(tx, call, &outcome).await;
                    }
                    ordered_results[idx] =
                        Some((call.name.clone(), call.tool_call_id.clone(), outcome));
                    continue;
                }
                crate::hooks::HookResult::Continue((name, args)) => {
                    tool_name = name;
                    tool_args = args;
                }
            }
        }

        maybe_inject_channel_delivery_defaults(
            &tool_name,
            &mut tool_args,
            ctx.channel_name,
            ctx.channel_reply_target,
        );

        crate::agent::set_runtime_approved_arg(&tool_name, &mut tool_args, false);

        let requires_prompt = ctx
            .approval
            .map(|mgr| mgr.needs_approval(&tool_name))
            .unwrap_or(false);
        let reentrant_agent_tool =
            crate::tools::REENTRANT_AGENT_TOOLS.contains(&tool_name.as_str());
        if requires_prompt && tool_name == "shell" && !reentrant_agent_tool {
            let prompt_signature = tool_call_signature(&tool_name, &tool_args);
            if !prompt_approval_tool_signatures_this_round.insert(prompt_signature.clone()) {
                let duplicate =
                    record_duplicate_tool_call(ctx, &tool_name, &tool_args, iteration).await;
                ordered_results[idx] =
                    Some((tool_name.clone(), call.tool_call_id.clone(), duplicate));
                continue;
            }
            if !prompt_approval_tool_signatures.insert(prompt_signature) {
                let repeated = format!(
                    "Agent loop aborted: repeated prompt-required tool call '{tool_name}' with identical arguments before approval."
                );
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "model": ctx.model,
                            "iteration": iteration + 1,
                            "tool": tool_name.clone(),
                            "arguments": scrub_credentials(&tool_args.to_string()),
                            "result": repeated,
                            "trace_id": ctx.turn_id,
                        })),
                    "tool_call_result"
                );
                if let Some(tx) = ctx.on_delta {
                    let _ = tx
                        .send(StreamDelta::Status(format!(
                            "\u{274c} {}: {}\n",
                            tool_name, repeated
                        )))
                        .await;
                }
                anyhow::bail!("{repeated}");
            }
        }

        // ── Cross-turn write-velocity gate ─────────────────
        // Counts would-execute mutating calls per (tenant, agent, tool) in
        // a sliding window that survives across turns. Past budget, the
        // call parks as a pending approval (same F-1 row the irreversible
        // tier produces) instead of executing — this is what stops a
        // once-per-turn "successful" loop the in-turn detector can never
        // see. Runs before the approval gate; denied/prompted/pending
        // calls are untouched and unconsumed.
        if let Some(parked) = super::velocity_gate::check_velocity_park(
            ctx,
            &tool_name,
            &tool_args,
            iteration,
        ) {
            if let super::approval_gate::ApprovalGateOutcome::Deny(outcome) = parked {
                if let Some(tx) = ctx.event_tx {
                    emit_tool_call_pair(tx, call, &outcome).await;
                }
                ordered_results[idx] =
                    Some((tool_name.clone(), call.tool_call_id.clone(), outcome));
                continue;
            }
        }

        // ── Approval hook ────────────────────────────────
        let approved = match gate_tool_approval(ctx, &tool_name, &tool_args, iteration).await {
            ApprovalGateOutcome::Proceed { approved } => approved,
            ApprovalGateOutcome::Deny(outcome) | ApprovalGateOutcome::Replace(outcome) => {
                // Streaming consumers see the denied/replaced call and its
                // synthesized result (e.g. a DenyWithEdit replacement) as a
                // ToolCall/ToolResult pair, as the direct path always did.
                if let Some(tx) = ctx.event_tx {
                    emit_tool_call_pair(tx, call, &outcome).await;
                }
                ordered_results[idx] =
                    Some((tool_name.clone(), call.tool_call_id.clone(), outcome));
                continue;
            }
        };
        crate::agent::set_runtime_approved_arg(&tool_name, &mut tool_args, approved);

        let signature = tool_call_signature(&tool_name, &tool_args);
        let dedup_exempt =
            ctx.dedup_exempt_tools.iter().any(|e| e == &tool_name) || reentrant_agent_tool;
        if dedup_enabled && !dedup_exempt && !seen_tool_signatures.insert(signature) {
            let duplicate =
                record_duplicate_tool_call(ctx, &tool_name, &tool_args, iteration).await;
            ordered_results[idx] = Some((tool_name.clone(), call.tool_call_id.clone(), duplicate));
            continue;
        }

        // ── Cerveau (enterprise-hardening round 1, F-2): idempotency claim
        // ─────────────────────────────────────────────────────────────
        // Distinct from the in-round duplicate check above: this guards
        // against a *replayed* call (a crashed turn re-entering with the
        // same history) re-firing a side-effectful tool, not a same-round
        // repeat. Only Safe-tier calls skip it; a manager with no ledger
        // configured (today's default) is a no-op, same as before this
        // round.
        let mut claimed_key: Option<String> = None;
        if let Some(mgr) = ctx.approval
            && let Some(ledger) = mgr.idem_ledger()
            && mgr.risk_tier(&tool_name) != zeroclaw_config::schema::ToolRiskTier::Safe
        {
            let principal = crate::agent::tenant::current_tenant()
                .map(|t| t.platform_user_id.clone())
                .unwrap_or_default();
            // No separate goal-task id exists at this layer (a plain chat/
            // webhook turn, not a goal-task) — turn_id doubles for both
            // positions. Documented simplification: sufficient today since
            // nothing yet replays a turn (F-1 auto-resume is unbuilt, ADR-003);
            // this is forward-looking infrastructure for when it lands.
            let key = crate::control_plane::tool_idem::derive_key(
                &principal,
                ctx.turn_id,
                ctx.turn_id,
                &tool_name,
                &tool_args.to_string(),
            );
            match ledger.claim(&key) {
                Ok(crate::control_plane::tool_idem::Claim::Claimed) => {
                    claimed_key = Some(key);
                }
                Ok(crate::control_plane::tool_idem::Claim::AlreadyDone(output)) => {
                    let outcome = ToolExecutionOutcome {
                        output: output.clone(),
                        success: true,
                        error_reason: None,
                        duration: Duration::ZERO,
                        receipt: None,
                        output_data: None,
                    };
                    if let Some(tx) = ctx.event_tx {
                        emit_tool_call_pair(tx, call, &outcome).await;
                    }
                    ordered_results[idx] =
                        Some((tool_name.clone(), call.tool_call_id.clone(), outcome));
                    continue;
                }
                Ok(crate::control_plane::tool_idem::Claim::InFlight) => {
                    let msg = format!(
                        "'{tool_name}' is already in progress from an earlier attempt \
                         (idempotency key in flight, no recorded success yet) — not \
                         re-executing to avoid a duplicate side effect."
                    );
                    let outcome = ToolExecutionOutcome {
                        output: msg.clone(),
                        success: false,
                        error_reason: Some(msg),
                        duration: Duration::ZERO,
                        receipt: None,
                        output_data: None,
                    };
                    if let Some(tx) = ctx.event_tx {
                        emit_tool_call_pair(tx, call, &outcome).await;
                    }
                    ordered_results[idx] =
                        Some((tool_name.clone(), call.tool_call_id.clone(), outcome));
                    continue;
                }
                Err(e) => {
                    // H4: ledger I/O failure fails CLOSED for mutating tools.
                    // This branch only runs for non-Safe tools, so executing
                    // anyway would fire a side effect with no replay
                    // protection at exactly the moment replays are likeliest
                    // (storage trouble). Deny with a retryable message; the
                    // next attempt claims normally. (Safe tools never reach
                    // here — reads stay available during a ledger outage.)
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"tool": tool_name, "error": e.to_string()})),
                        "F-2 idempotency claim failed; refusing to run unprotected"
                    );
                    let msg = format!(
                        "'{tool_name}' cannot run right now: the idempotency \
                         ledger is unavailable, so an unprotected execution \
                         could duplicate a side effect. Retry this call shortly."
                    );
                    let outcome = ToolExecutionOutcome {
                        output: msg.clone(),
                        success: false,
                        error_reason: Some(msg),
                        duration: Duration::ZERO,
                        receipt: None,
                        output_data: None,
                    };
                    if let Some(tx) = ctx.event_tx {
                        emit_tool_call_pair(tx, call, &outcome).await;
                    }
                    ordered_results[idx] =
                        Some((tool_name.clone(), call.tool_call_id.clone(), outcome));
                    continue;
                }
            }
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_attrs(::serde_json::json!({
                    "model": ctx.model,
                    "iteration": iteration + 1,
                    "tool": tool_name.clone(),
                    "arguments": scrub_credentials(&tool_args.to_string()),
                    "trace_id": ctx.turn_id,
                })),
            "tool_call_start"
        );

        // ── Progress: tool start ────────────────────────────
        if let Some(tx) = ctx.on_delta {
            let hint = {
                let raw = match tool_name.as_str() {
                    "shell" => tool_args.get("command").and_then(|v| v.as_str()),
                    "file_read" | "file_write" => tool_args.get("path").and_then(|v| v.as_str()),
                    _ => tool_args
                        .get("action")
                        .and_then(|v| v.as_str())
                        .or_else(|| tool_args.get("query").and_then(|v| v.as_str())),
                };
                match raw {
                    Some(s) => truncate_with_ellipsis(s, 60),
                    None => String::new(),
                }
            };
            let progress = if hint.is_empty() {
                format!("\u{23f3} {}\n", tool_name)
            } else {
                format!("\u{23f3} {}: {hint}\n", tool_name)
            };
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_attrs(::serde_json::json!({"tool": tool_name})),
                "Sending progress start to draft"
            );
            let _ = tx.send(StreamDelta::Status(progress)).await;
        }

        executable_indices.push(idx);
        let call_id = super::events::resolve_tool_call_id(&ParsedToolCall {
            name: tool_name.clone(),
            arguments: tool_args.clone(),
            tool_call_id: call.tool_call_id.clone(),
            // Calls with a parse error were already rejected and `continue`d
            // above; everything reaching this point parsed successfully.
            arguments_parse_error: None,
        });
        // Pin the resolved id onto the executable call so the pending ToolCall
        // and the terminal ToolResult (both emitted by the executor at dispatch
        // and completion) share one correlation id, even for id-less
        // text-protocol calls.
        executable_calls.push(ParsedToolCall {
            name: tool_name,
            arguments: tool_args,
            tool_call_id: Some(call_id),
            arguments_parse_error: None,
        });
        claimed_idem_keys.push(claimed_key);
    }

    Ok(PreparedToolCalls {
        ordered_results,
        executable_indices,
        executable_calls,
        claimed_idem_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::NoopObserver;

    fn test_ctx<'a>(turn_id: &'a str, dedup_exempt_tools: &'a [String], pacing: &'a zeroclaw_config::schema::PacingConfig) -> TurnCtx<'a> {
        TurnCtx {
            observer: &NoopObserver,
            provider_name: "testprov",
            model: "testmodel",
            temperature: None,
            approval: None,
            channel_name: "",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            event_tx: None,
            hooks: None,
            dedup_exempt_tools,
            pacing,
            strict_tool_parsing: false,
            channel: None,
            turn_id,
            agent_alias: None,
            parent_agent_alias: None,
        }
    }

    /// Regression for the "silent empty-object" bug: a tool call whose
    /// `arguments` failed to parse as JSON (flagged via
    /// `arguments_parse_error` by the response parser) must be rejected
    /// outright — never executed with a substituted `{}` — and must report
    /// an explicit failure back to the model, addressed to the original
    /// tool_call_id so provider role-alternation stays valid.
    #[tokio::test]
    async fn prepare_tool_calls_rejects_call_with_unparsable_arguments_without_executing() {
        let dedup_exempt_tools: Vec<String> = Vec::new();
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let ctx = test_ctx("turn-arg-parse-error", &dedup_exempt_tools, &pacing);

        let calls = vec![ParsedToolCall {
            name: "file_write".to_string(),
            arguments: serde_json::Value::Object(serde_json::Map::new()),
            tool_call_id: Some("call_1".to_string()),
            arguments_parse_error: Some(
                "failed to parse tool arguments as JSON: EOF while parsing an object".to_string(),
            ),
        }];

        let mut seen_tool_signatures = HashSet::new();
        let mut prompt_approval_tool_signatures = HashSet::new();

        let prepared = prepare_tool_calls(
            &ctx,
            &calls,
            &mut seen_tool_signatures,
            &mut prompt_approval_tool_signatures,
            0,
            true,
        )
        .await
        .expect("prepare_tool_calls should not error");

        // The call must never become executable — it must not run with the
        // substituted empty-object arguments.
        assert!(
            prepared.executable_indices.is_empty(),
            "a call with unparsable arguments must not be scheduled for execution"
        );
        assert!(prepared.executable_calls.is_empty());

        // It must instead carry an explicit failure result, addressed to the
        // same tool_call_id, so a role=tool follow-up message still lines up
        // with the assistant's claimed tool call.
        let mut ordered_results = prepared.ordered_results;
        let (name, tool_call_id, outcome) =
            ordered_results.remove(0).expect("a result must be recorded");
        assert_eq!(name, "file_write");
        assert_eq!(tool_call_id.as_deref(), Some("call_1"));
        assert!(!outcome.success, "the outcome must be a failure");
        assert!(
            outcome.output.contains("file_write"),
            "failure output should name the tool: {}",
            outcome.output
        );
        assert!(
            outcome
                .error_reason
                .as_deref()
                .unwrap_or_default()
                .contains("failed to parse tool arguments as JSON"),
            "error_reason should carry the parse failure: {:?}",
            outcome.error_reason
        );
    }

    fn breaker_call(name: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: name.to_string(),
            arguments: serde_json::json!({"thread_id": "t-1"}),
            tool_call_id: Some("call_b".to_string()),
            arguments_parse_error: None,
        }
    }

    fn outcome(success: bool, text: &str) -> crate::agent::tool_execution::ToolExecutionOutcome {
        crate::agent::tool_execution::ToolExecutionOutcome {
            output: text.to_string(),
            success,
            error_reason: (!success).then(|| text.to_string()),
            duration: std::time::Duration::ZERO,
            receipt: None,
            output_data: None,
        }
    }

    /// Feed `n` executed outcomes for `tool` through the real post-execution path.
    async fn feed_outcomes(
        ctx: &TurnCtx<'_>,
        tool: &str,
        n: usize,
        success: bool,
        text: &str,
    ) {
        for _ in 0..n {
            let calls = vec![breaker_call(tool)];
            let mut ordered = vec![None];
            crate::agent::turn::post_exec::record_executed_outcomes(
                ctx,
                &[0],
                &calls,
                vec![outcome(success, text)],
                &[None],
                &mut ordered,
                0,
            )
            .await;
        }
    }

    async fn prepared_for(ctx: &TurnCtx<'_>, tool: &str) -> PreparedToolCalls {
        let calls = vec![breaker_call(tool)];
        prepare_tool_calls(
            ctx,
            &calls,
            &mut HashSet::new(),
            &mut HashSet::new(),
            0,
            false,
        )
        .await
        .expect("prepare_tool_calls should not error")
    }

    #[tokio::test]
    async fn a_dead_mcp_server_is_short_circuited_after_five_server_failures() {
        let dedup_exempt_tools: Vec<String> = Vec::new();
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let ctx = test_ctx("turn-breaker-open", &dedup_exempt_tools, &pacing);
        let tool = "brk_dead__get_thread_memory";

        feed_outcomes(
            &ctx,
            tool,
            5,
            false,
            "MCP server `brk_dead` error during tool call `get_thread_memory`: HTTP 502",
        )
        .await;

        let prepared = prepared_for(&ctx, tool).await;
        assert!(prepared.executable_indices.is_empty(), "must not reach the dead server");
        let (_, id, blocked) = prepared.ordered_results.into_iter().next().flatten().expect("result");
        assert_eq!(id.as_deref(), Some("call_b"));
        assert!(!blocked.success);
        assert!(blocked.output.contains("temporarily unavailable"), "{}", blocked.output);
        assert!(blocked.output.contains("HTTP 502"), "cause must reach the model: {}", blocked.output);
        assert!(blocked.output.contains("Do not retry"), "{}", blocked.output);

        // A different tool of the same server-shaped name is unaffected.
        let other = prepared_for(&ctx, "brk_dead__search_mail").await;
        assert_eq!(other.executable_indices, vec![0]);
    }

    #[tokio::test]
    async fn answers_with_errors_or_successes_never_open_the_breaker() {
        let dedup_exempt_tools: Vec<String> = Vec::new();
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let ctx = test_ctx("turn-breaker-closed", &dedup_exempt_tools, &pacing);

        // The server ANSWERS with an error every time ("not found"): it is alive.
        let answering = "brk_alive__get_thread";
        feed_outcomes(
            &ctx,
            answering,
            8,
            false,
            "MCP `get_thread` (server `brk_alive`) returned isError: thread not found",
        )
        .await;
        assert_eq!(prepared_for(&ctx, answering).await.executable_indices, vec![0]);

        // Four server failures, one success, four more: never five in a row.
        let flaky = "brk_flaky__search";
        let down = "MCP server `brk_flaky` timed out after 30s before writing tool call `search`";
        feed_outcomes(&ctx, flaky, 4, false, down).await;
        feed_outcomes(&ctx, flaky, 1, true, "ok").await;
        feed_outcomes(&ctx, flaky, 4, false, down).await;
        assert_eq!(prepared_for(&ctx, flaky).await.executable_indices, vec![0]);
    }

    #[tokio::test]
    async fn breaker_threshold_zero_disables_it_and_builtins_are_exempt() {
        let dedup_exempt_tools: Vec<String> = Vec::new();
        let mut pacing = zeroclaw_config::schema::PacingConfig::default();
        pacing.tool_breaker_threshold = 0;
        let ctx = test_ctx("turn-breaker-off", &dedup_exempt_tools, &pacing);
        let down = "MCP server `brk_off` error during tool call `t`";
        feed_outcomes(&ctx, "brk_off__t", 10, false, down).await;
        assert_eq!(prepared_for(&ctx, "brk_off__t").await.executable_indices, vec![0]);

        // Built-ins (no `server__tool` shape) have no remote server to be down.
        let pacing_on = zeroclaw_config::schema::PacingConfig::default();
        let ctx_on = test_ctx("turn-breaker-builtin", &dedup_exempt_tools, &pacing_on);
        feed_outcomes(&ctx_on, "brk_builtin", 10, false, down).await;
        assert_eq!(prepared_for(&ctx_on, "brk_builtin").await.executable_indices, vec![0]);
    }

    /// A normal, successfully-parsed call must be unaffected by the new
    /// rejection branch and still reach the executable set.
    #[tokio::test]
    async fn prepare_tool_calls_still_executes_calls_with_valid_arguments() {
        let dedup_exempt_tools: Vec<String> = Vec::new();
        let pacing = zeroclaw_config::schema::PacingConfig::default();
        let ctx = test_ctx("turn-valid-args", &dedup_exempt_tools, &pacing);

        let calls = vec![ParsedToolCall {
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "echo hi"}),
            tool_call_id: Some("call_2".to_string()),
            arguments_parse_error: None,
        }];

        let mut seen_tool_signatures = HashSet::new();
        let mut prompt_approval_tool_signatures = HashSet::new();

        let prepared = prepare_tool_calls(
            &ctx,
            &calls,
            &mut seen_tool_signatures,
            &mut prompt_approval_tool_signatures,
            0,
            true,
        )
        .await
        .expect("prepare_tool_calls should not error");

        assert_eq!(prepared.executable_indices, vec![0]);
        assert_eq!(prepared.executable_calls.len(), 1);
        assert_eq!(prepared.executable_calls[0].name, "shell");
        assert!(prepared.ordered_results[0].is_none());
    }
}
