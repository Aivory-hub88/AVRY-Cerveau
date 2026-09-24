//! The max-iteration exit: when the loop exhausts its iterations, ask the
//! LLM for a tools-free final summary (with step timeout + cancel select)
//! and return it appended to the accumulated display text, or bail.

use super::knobs::{LoopKnobs, MaxIterationBehavior};
use super::outcome::ToolLoopCancelled;
use super::redact::scrub_credentials;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_config::schema::PacingConfig;
use zeroclaw_providers::{ChatMessage, ModelProvider};
use zeroclaw_tool_call_parser::{strip_think_tags, strip_trailing_terminal_markers};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_after_max_iterations(
    model_provider: &dyn ModelProvider,
    history: &mut Vec<ChatMessage>,
    provider_name: &str,
    model: &str,
    temperature: Option<f64>,
    pacing: &PacingConfig,
    cancellation_token: Option<&CancellationToken>,
    max_iterations: usize,
    mut accumulated_display_text: String,
    turn_id: &str,
    knobs: &LoopKnobs,
    event_tx: Option<&Sender<TurnEvent>>,
    mut new_messages_out: Option<&mut Vec<ChatMessage>>,
) -> Result<String> {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "model": model,
                "max_iterations": max_iterations,
                "trace_id": turn_id,
            })),
        "tool_loop_exhausted"
    );

    // ErrorAtCap callers (embedders driving Agent::turn) treat the cap as a
    // control signal: bail instead of spending another LLM call on a summary.
    if knobs.max_iteration_behavior == MaxIterationBehavior::ErrorAtCap {
        anyhow::bail!("Agent exceeded maximum tool iterations ({max_iterations})")
    }

    // Graceful shutdown: ask the LLM for a final summary without tools
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({"max_iterations": max_iterations})),
        "Max iterations reached, requesting final summary"
    );
    let tool_calls_stripped =
        crate::agent::history_pruner::strip_orphaned_tool_calls_from_assistants(history);
    let tool_messages_removed =
        crate::agent::history_pruner::remove_orphaned_tool_messages(history).removed;
    if tool_calls_stripped > 0 || tool_messages_removed > 0 {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "tool_calls_stripped": tool_calls_stripped,
                    "tool_messages_removed": tool_messages_removed,
                })),
            "Sanitised orphaned tool_use/tool_result pairing before graceful shutdown"
        );
    }

    let summary_prompt = ChatMessage::user(
        "You have reached the maximum number of tool iterations. \
         Please provide your best answer based on the work completed so far. \
         Summarize what you accomplished and what remains to be done."
            .to_string(),
    );
    let summary_prompt_mirror = summary_prompt.clone();
    history.push(summary_prompt);

    enum SummaryCall {
        Cancelled,
        TimedOut(u64),
        Done(Result<zeroclaw_providers::ChatResponse>),
    }
    let summary_call = {
        let summary_request = zeroclaw_providers::ChatRequest {
            messages: history,
            tools: None, // No tools — force a text response
            thinking: zeroclaw_api::NATIVE_THINKING_OVERRIDE
                .try_with(Clone::clone)
                .ok()
                .flatten(),
        };
        let access = crate::agent::turn::execution::ResolvedModelAccess {
            model_provider,
            provider_name,
            model,
            temperature,
        };
        // Route the graceful-summary call through the metered provider seam. This
        // was the one tool-loop provider call that skipped the budget check and
        // recorded no cost; through the seam it now fails closed when the turn's
        // budget is exhausted and its token usage is charged like any in-loop
        // call. Metering is a no-op when the turn is unscoped.
        let summary_future = access.run_model_query(summary_request);
        match pacing.step_timeout_secs {
            Some(step_secs) if step_secs > 0 => {
                let step_timeout = Duration::from_secs(step_secs);
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => SummaryCall::Cancelled,
                        result = tokio::time::timeout(step_timeout, summary_future) => match result {
                            Ok(inner) => SummaryCall::Done(inner),
                            Err(_) => SummaryCall::TimedOut(step_secs),
                        },
                    }
                } else {
                    match tokio::time::timeout(step_timeout, summary_future).await {
                        Ok(inner) => SummaryCall::Done(inner),
                        Err(_) => SummaryCall::TimedOut(step_secs),
                    }
                }
            }
            _ => {
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => SummaryCall::Cancelled,
                        result = summary_future => SummaryCall::Done(result),
                    }
                } else {
                    SummaryCall::Done(summary_future.await)
                }
            }
        }
    };

    let resp = match summary_call {
        SummaryCall::Cancelled => {
            history.pop();
            return Err(ToolLoopCancelled.into());
        }
        SummaryCall::TimedOut(step_secs) => {
            history.pop();
            anyhow::bail!("Final summary LLM call timed out after {step_secs}s (step_timeout_secs)")
        }
        SummaryCall::Done(Err(e)) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "provider": provider_name,
                        "max_iterations": max_iterations,
                        "trace_id": turn_id,
                        "error": format!("{e}"),
                    })),
                "final summary LLM call failed after iteration exhaustion; bailing"
            );
            history.pop();
            return Err(e).context(format!(
                "Agent exceeded maximum tool iterations ({max_iterations})"
            ));
        }
        SummaryCall::Done(Ok(resp)) => resp,
    };

    let raw_text = resp.text.unwrap_or_default();
    if raw_text.is_empty() {
        history.pop();
        anyhow::bail!("Agent exceeded maximum tool iterations ({max_iterations})")
    }
    // The summary is raw provider text, and emitting it as a chunk makes this
    // a new automatic display sink: ACP renders `agent_message_chunk` live,
    // while gateway and RPC forward the same chunk without another
    // normalization step. Apply the display hygiene the ordinary final
    // response already gets before anything is emitted -- strip hidden think
    // content and trailing terminal markers, then withhold text that looks
    // like an internal tool-protocol envelope. The summary call passes
    // `tools: None`, so the tools-free detector is the matching contract.
    let display_text = strip_trailing_terminal_markers(&strip_think_tags(&raw_text));
    let protocol_suppressed =
        super::protocol_detect::detect_internal_protocol_without_tools(&display_text).is_some();
    if protocol_suppressed {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "model": model,
                    "max_iterations": max_iterations,
                    "trace_id": turn_id,
                    "error": "malformed internal tool protocol omitted from max-iteration summary",
                })),
            "max_iteration_summary_protocol_suppressed"
        );
    }
    let display_text = if protocol_suppressed {
        crate::i18n::get_required_cli_string("channel-runtime-malformed-tool-output").to_string()
    } else {
        display_text
    };
    if display_text.trim().is_empty() {
        history.pop();
        anyhow::bail!("Agent exceeded maximum tool iterations ({max_iterations})")
    }
    // History and result payloads keep the unmodified provider text; only the
    // display path is normalized, matching the final-response contract.
    let summary_msg = ChatMessage::assistant(raw_text.clone());
    if let Some(out) = &mut new_messages_out {
        out.push(summary_prompt_mirror);
        out.push(summary_msg.clone());
    }
    history.push(summary_msg);
    // Graceful shutdown with a visible reason so the user knows why the
    // agent stopped making progress.
    let stop_reason = crate::i18n::get_required_cli_string_with_args(
        "turn-max-iterations-reached",
        &[("max_iterations", &max_iterations.to_string())],
    );
    let segment = format!("{display_text}\n\n{stop_reason}");
    // This summary is the turn's only visible output on the max-iteration
    // exit path, and it comes from a fresh non-streaming call — there is no
    // live delta a client could have already received, so unlike the normal
    // final-response path this emit needs no live-vs-post-hoc guard. Only the
    // newly-produced segment goes out; `accumulated_display_text` holds
    // narration earlier iterations already streamed, and resending it here
    // would duplicate it in the client.
    super::events::emit_posthoc_turn_chunk(event_tx, &segment).await;
    accumulated_display_text.push_str(&segment);
    Ok(accumulated_display_text)
}

/// Graceful loop-break exit: the loop detector (or identical-output abort)
/// stopped the turn mid-flight. Same tools-free summary machinery as the
/// max-iteration exit, but the stop reason names the detector trip instead
/// of the iteration cap — and unlike the cap path, this NEVER fails the
/// turn: if the summary call itself fails, the caller still gets the
/// partial work plus an honest note instead of a 500.
///
/// Display-hygiene parity with the max-iteration exit is load-bearing here:
/// the summary is user-facing, so hidden think content and trailing terminal
/// markers are stripped and an internal tool-protocol envelope is suppressed
/// rather than rendered (upstream #10026 applied this to the cap path only).
///
/// Trace parity matters as much: the wrap-up summary call runs through the
/// metered provider seam, bypassing the turn loop's `llm_request`/
/// `llm_response` events, and this exit returns the turn's final text without
/// a `turn_final_response` event — without both, traces go dark exactly when
/// the detector fires. All three are emitted here, tagged `"wrap_up":
/// "loop_break"` so they never masquerade as an ordinary iteration.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_after_loop_break(
    model_provider: &dyn ModelProvider,
    history: &mut Vec<ChatMessage>,
    provider_name: &str,
    model: &str,
    temperature: Option<f64>,
    pacing: &PacingConfig,
    cancellation_token: Option<&CancellationToken>,
    break_message: &str,
    mut accumulated_display_text: String,
    turn_id: &str,
    iteration: usize,
    knobs: &LoopKnobs,
    new_messages_out: Option<&mut Vec<ChatMessage>>,
) -> Result<String> {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "model": model,
                "break_message": break_message,
                "trace_id": turn_id,
            })),
        "tool loop broken by detector, requesting graceful wrap-up"
    );

    // ErrorAtCap callers treat any cap as a control signal.
    if knobs.max_iteration_behavior == MaxIterationBehavior::ErrorAtCap {
        anyhow::bail!("Agent loop aborted by loop detector: {break_message}")
    }

    let tool_calls_stripped =
        crate::agent::history_pruner::strip_orphaned_tool_calls_from_assistants(history);
    let tool_messages_removed =
        crate::agent::history_pruner::remove_orphaned_tool_messages(history).removed;
    if tool_calls_stripped > 0 || tool_messages_removed > 0 {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "tool_calls_stripped": tool_calls_stripped,
                    "tool_messages_removed": tool_messages_removed,
                })),
            "Sanitised orphaned tool_use/tool_result pairing before graceful shutdown"
        );
    }

    let summary_prompt = ChatMessage::user(format!(
        "The automated loop guard stopped the tool loop early: {break_message}. \
             No more tool calls from here. Write your final user-facing answer now: \
             what was accomplished, what is still pending, and what you need \
             to continue. Be concrete and brief."
    ));
    let summary_prompt_mirror = summary_prompt.clone();
    history.push(summary_prompt);

    let stop_note = format!("Stopped early by the loop guard: {break_message}.");

    // The wrap-up call bypasses the turn loop's provider-call path, so its
    // `llm_request` would never be recorded. Emit it here in the same shape
    // (tagged as wrap-up, attributed to the breaking iteration).
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Send)
            .with_category(::zeroclaw_log::EventCategory::Provider)
            .with_attrs(::serde_json::json!({
                "model": model,
                "iteration": iteration,
                "messages_count": history.len(),
                "wrap_up": "loop_break",
                "trace_id": turn_id,
            })),
        "llm_request"
    );
    let wrap_up_started_at = Instant::now();
    let wrap_up_elapsed_ms =
        || u64::try_from(wrap_up_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

    enum SummaryCall {
        Cancelled,
        TimedOut(u64),
        Done(Result<zeroclaw_providers::ChatResponse>),
    }
    let summary_call = {
        let summary_request = zeroclaw_providers::ChatRequest {
            messages: history,
            tools: None, // No tools — force a text response
            thinking: zeroclaw_api::NATIVE_THINKING_OVERRIDE
                .try_with(Clone::clone)
                .ok()
                .flatten(),
        };
        let access = crate::agent::turn::execution::ResolvedModelAccess {
            model_provider,
            provider_name,
            model,
            temperature,
        };
        let summary_future = access.run_model_query(summary_request);
        match pacing.step_timeout_secs {
            Some(step_secs) if step_secs > 0 => {
                let step_timeout = Duration::from_secs(step_secs);
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => SummaryCall::Cancelled,
                        result = tokio::time::timeout(step_timeout, summary_future) => match result {
                            Ok(inner) => SummaryCall::Done(inner),
                            Err(_) => SummaryCall::TimedOut(step_secs),
                        },
                    }
                } else {
                    match tokio::time::timeout(step_timeout, summary_future).await {
                        Ok(inner) => SummaryCall::Done(inner),
                        Err(_) => SummaryCall::TimedOut(step_secs),
                    }
                }
            }
            _ => {
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => SummaryCall::Cancelled,
                        result = summary_future => SummaryCall::Done(result),
                    }
                } else {
                    SummaryCall::Done(summary_future.await)
                }
            }
        }
    };

    // Unlike the cap path, a failed wrap-up must not fail the turn: the
    // user keeps the partial work plus the stop reason.
    let mut canned_fallback = || -> anyhow::Result<String> {
        history.pop();
        if !accumulated_display_text.is_empty() {
            accumulated_display_text.push_str("\n\n");
        }
        accumulated_display_text.push_str(&stop_note);
        accumulated_display_text
            .push_str(" The closing summary could not be generated, but the work above stands.");
        Ok(accumulated_display_text.clone())
    };
    // This exit returns the turn's final text, so it owns the
    // `turn_final_response` event the normal path emits in the loop.
    let emit_final = |text: &str| {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "model": model,
                    "iteration": iteration,
                    "wrap_up": "loop_break",
                    "text": scrub_credentials(text),
                    "trace_id": turn_id,
                })),
            "turn_final_response"
        );
    };
    let resp = match summary_call {
        SummaryCall::Cancelled => return Err(ToolLoopCancelled.into()),
        SummaryCall::TimedOut(step_secs) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "provider": provider_name,
                        "trace_id": turn_id,
                        "error": format!("wrap-up timed out after {step_secs}s"),
                    })),
                "loop-break wrap-up timed out; returning partial work"
            );
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_duration(wrap_up_elapsed_ms())
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "iteration": iteration,
                        "wrap_up": "loop_break",
                        "error": format!("wrap-up timed out after {step_secs}s"),
                        "trace_id": turn_id,
                    })),
                "llm_response"
            );
            let out = canned_fallback()?;
            emit_final(&out);
            return Ok(out);
        }
        SummaryCall::Done(Err(e)) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "provider": provider_name,
                        "trace_id": turn_id,
                        "error": format!("{e}"),
                    })),
                "loop-break wrap-up call failed; returning partial work"
            );
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Provider)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_duration(wrap_up_elapsed_ms())
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "iteration": iteration,
                        "wrap_up": "loop_break",
                        "error": scrub_credentials(&e.to_string()),
                        "trace_id": turn_id,
                    })),
                "llm_response"
            );
            let out = canned_fallback()?;
            emit_final(&out);
            return Ok(out);
        }
        SummaryCall::Done(Ok(resp)) => resp,
    };

    let input_tokens = resp.usage.as_ref().and_then(|usage| usage.input_tokens);
    let output_tokens = resp.usage.as_ref().and_then(|usage| usage.output_tokens);
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Receive)
            .with_category(::zeroclaw_log::EventCategory::Provider)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_duration(wrap_up_elapsed_ms())
            .with_attrs(::serde_json::json!({
                "model": model,
                "iteration": iteration,
                "wrap_up": "loop_break",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "raw_response": scrub_credentials(resp.text.as_deref().unwrap_or_default()),
                "native_tool_calls": resp.tool_calls.len(),
                "parsed_tool_calls": 0,
                "trace_id": turn_id,
            })),
        "llm_response"
    );

    let raw_text = resp.text.unwrap_or_default();
    if raw_text.is_empty() {
        let out = canned_fallback()?;
        emit_final(&out);
        return Ok(out);
    }
    // Same display-safe contract as the max-iteration exit: strip hidden
    // think content and trailing terminal markers, then withhold text that
    // looks like an internal tool-protocol envelope. History keeps the raw
    // provider text; only the display path is normalized.
    let display_text = strip_trailing_terminal_markers(&strip_think_tags(&raw_text));
    let protocol_suppressed =
        super::protocol_detect::detect_internal_protocol_without_tools(&display_text).is_some();
    if protocol_suppressed {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "model": model,
                    "trace_id": turn_id,
                    "error": "malformed internal tool protocol omitted from loop-break summary",
                })),
            "loop_break_summary_protocol_suppressed"
        );
    }
    let display_text = if protocol_suppressed {
        crate::i18n::get_required_cli_string("channel-runtime-malformed-tool-output").to_string()
    } else {
        display_text
    };
    if display_text.trim().is_empty() {
        let out = canned_fallback()?;
        emit_final(&out);
        return Ok(out);
    }
    let summary_msg = ChatMessage::assistant(raw_text.clone());
    if let Some(out) = new_messages_out {
        out.push(summary_prompt_mirror);
        out.push(summary_msg.clone());
    }
    history.push(summary_msg);
    if !accumulated_display_text.is_empty() {
        accumulated_display_text.push_str("\n\n");
    }
    accumulated_display_text.push_str(&display_text);
    accumulated_display_text.push_str("\n\n");
    accumulated_display_text.push_str(&stop_note);
    emit_final(&accumulated_display_text);
    Ok(accumulated_display_text)
}

#[cfg(test)]
mod graceful_summary_metering_tests {
    use super::finish_after_max_iterations;
    use crate::agent::cost::{TOOL_LOOP_COST_TRACKING_CONTEXT, ToolLoopCostTrackingContext};
    use crate::agent::turn::LoopKnobs;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::model_provider::{
        ChatRequest, ChatResponse, SemanticEmptyTerminalCompletion,
    };
    use zeroclaw_config::schema::{CostConfig, PacingConfig};
    use zeroclaw_providers::traits::TokenUsage;
    use zeroclaw_providers::{ChatMessage, ModelProvider};

    use super::{Sender, TurnEvent};

    /// Provider stub that counts calls and returns a summary WITH token usage.
    struct CountingUsageProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for CountingUsageProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("wrap-up summary".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                text: Some("wrap-up summary".to_string()),
                tool_calls: Vec::new(),
                usage: Some(TokenUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    cached_input_tokens: None,
                }),
                reasoning_content: None,
            })
        }
    }

    impl Attributable for CountingUsageProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "counting-usage-provider"
        }
    }

    async fn run_summary_with_events(
        provider: &dyn ModelProvider,
        accumulated_display_text: String,
        event_tx: Option<&Sender<TurnEvent>>,
    ) -> anyhow::Result<String> {
        let mut history = vec![ChatMessage::user("do the work")];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default(); // GracefulSummary
        finish_after_max_iterations(
            provider,
            &mut history,
            "custom",
            "test-model",
            None,
            &pacing,
            None,
            2,
            accumulated_display_text,
            "trace-req-test",
            &knobs,
            event_tx,
            None,
        )
        .await
    }

    async fn run_summary(provider: &dyn ModelProvider) -> anyhow::Result<String> {
        run_summary_with_events(provider, String::new(), None).await
    }

    // The graceful summary now routes through the metered provider seam: under a
    // cost-tracking scope its token usage is recorded (before this change the
    // summary recorded nothing).
    #[tokio::test]
    async fn graceful_summary_records_usage_through_the_metered_seam() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingUsageProvider {
            calls: Arc::clone(&calls),
        };
        let ctx = ToolLoopCostTrackingContext::usage_only();
        let turn_usage = Arc::clone(&ctx.turn_usage);

        let out = TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(Some(ctx), async { run_summary(&provider).await })
            .await
            .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        // The returned display text must carry both the summary and the visible
        // stop reason — deleting the stop-reason append would leave this green
        // on `wrap-up summary` alone, so the stop-reason assertion pins the
        // user-observed contract.
        assert!(
            out.contains("Turn stopped: reached maximum tool iterations (2)"),
            "stop reason with iteration count must reach returned output: {out}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "provider called once");
        let recorded = *turn_usage.lock();
        assert_eq!(recorded.input_tokens, 100);
        assert_eq!(recorded.output_tokens, 20);
    }

    // The graceful summary now fails closed on budget exhaustion: it was the one
    // tool-loop provider call that skipped the budget check. A tripped budget
    // (negative limit) makes the seam bail BEFORE spending, so the provider is
    // never called and the cap is surfaced as an error.
    #[tokio::test]
    async fn graceful_summary_is_budget_gated_and_skips_the_provider_when_over_budget() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingUsageProvider {
            calls: Arc::clone(&calls),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = CostConfig {
            enabled: true,
            daily_limit_usd: -1.0,
            monthly_limit_usd: -1.0,
            ..CostConfig::default()
        };
        let tracker = Arc::new(crate::cost::CostTracker::new(cfg, tmp.path()).unwrap());
        let ctx = ToolLoopCostTrackingContext::new(tracker, Arc::new(HashMap::new()));

        let result = TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(Some(ctx), async { run_summary(&provider).await })
            .await;

        assert!(result.is_err(), "over-budget summary must bail, not spend");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "budget gate must fire before the provider call"
        );
    }

    struct SemanticEmptySummaryProvider;

    #[async_trait]
    impl ModelProvider for SemanticEmptySummaryProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("unused")
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("<think>internal reasoning</think>".to_string()),
                tool_calls: Vec::new(),
                usage: Some(TokenUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    cached_input_tokens: None,
                }),
                reasoning_content: Some("internal reasoning".to_string()),
            })
        }
    }

    impl Attributable for SemanticEmptySummaryProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }

        fn alias(&self) -> &str {
            "semantic-empty-summary-provider"
        }
    }

    #[tokio::test]
    async fn graceful_summary_rejects_think_only_text_with_rejected_usage_and_typed_cause() {
        let provider = SemanticEmptySummaryProvider;
        let ctx = ToolLoopCostTrackingContext::usage_only();
        let turn_usage = Arc::clone(&ctx.turn_usage);

        let error = TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(Some(ctx), async {
                run_summary(&provider)
                    .await
                    .expect_err("think-only summary cannot be a successful terminal answer")
            })
            .await;

        assert!(
            error
                .chain()
                .any(|cause| cause.is::<SemanticEmptyTerminalCompletion>())
        );
        assert!(
            error
                .to_string()
                .contains("Agent exceeded maximum tool iterations (2)"),
            "the iteration cap remains the caller-visible summary failure: {error}"
        );
        let recorded = *turn_usage.lock();
        assert_eq!(recorded.input_tokens, 100);
        assert_eq!(recorded.output_tokens, 20);
        assert_eq!(recorded.last_input_tokens, 0);
    }

    /// Provider stub that records the exact messages it was dispatched, so a
    /// test can assert on what actually reached the provider.
    struct CapturingProvider {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ModelProvider for CapturingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            let joined = request
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen.lock().unwrap().push(joined);
            Ok(ChatResponse {
                text: Some("wrap-up summary".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl Attributable for CapturingProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "capturing-provider"
        }
    }

    // The graceful-summary path dispatches the accumulated history directly
    // through `run_model_query`, which does NOT run
    // `prepare_messages_for_provider`. A tool-result `[AUDIO:/path]` in that
    // history must be stripped before it reaches the provider, or the raw
    // filesystem path leaks and is hallucinated over on the max-iteration exit.
    #[tokio::test]
    async fn graceful_summary_strips_tool_audio_marker_before_dispatch() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
        };
        // A properly paired assistant tool_call + native tool-result JSON blob,
        // so the orphaned-tool-message sweep in finish_after_max_iterations keeps
        // the exchange intact and the audio marker survives to dispatch. This
        // also exercises stripping a marker embedded inside a tool-result JSON
        // object (the native-dispatcher shape), not just plain text.
        let mut history = vec![
            ChatMessage::user("call the tool and tell me what you hear"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_1"}]}"#),
            ChatMessage::tool(
                r#"{"content":"[AUDIO:/tmp/clip.wav] recorded 3:00 PM","tool_call_id":"toolu_1"}"#,
            ),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();

        let out = finish_after_max_iterations(
            &provider,
            &mut history,
            "custom",
            "test-model",
            None,
            &pacing,
            None,
            2,
            String::new(),
            "trace-req-audio",
            &knobs,
            None,
            None,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            !captured.contains("/tmp/clip.wav"),
            "raw audio path reached the provider on the max-iteration path: {captured}"
        );
        assert!(
            captured.contains("[media attachment]"),
            "audio marker should be replaced with a placeholder: {captured}"
        );
    }

    // ACP and other event-driven clients render message content exclusively
    // from `TurnEvent::Chunk`. The max-iteration exit must emit one, and it
    // must carry only the newly-produced segment — narration from earlier
    // iterations already reached the client through prior chunks, so
    // re-sending `accumulated_display_text` here would duplicate it.
    #[tokio::test]
    async fn graceful_summary_emits_a_turn_event_chunk_with_only_the_new_segment() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TurnEvent>(8);

        let out = run_summary_with_events(&provider, "earlier narration".to_string(), Some(&tx))
            .await
            .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");

        let mut chunk_delta = None;
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Chunk { delta } = event {
                chunk_delta = Some(delta);
            }
        }
        let delta = chunk_delta.expect("max-iteration exit must emit a TurnEvent::Chunk");
        assert!(
            delta.contains("wrap-up summary"),
            "chunk must carry the summary text: {delta}"
        );
        assert!(
            delta.contains("Turn stopped: reached maximum tool iterations (2)"),
            "chunk must carry the max-iterations stop reason: {delta}"
        );
        assert!(
            !delta.contains("earlier narration"),
            "chunk must not re-send narration already streamed in earlier iterations: {delta}"
        );
    }

    /// Provider stub returning caller-supplied raw summary text, so a test can
    /// drive the display-hygiene path with think content, terminal markers, or
    /// an internal tool-protocol envelope.
    struct RawTextProvider {
        text: String,
    }

    #[async_trait]
    impl ModelProvider for RawTextProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(self.text.clone())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(self.text.clone()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl Attributable for RawTextProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "raw-text-provider"
        }
    }

    async fn emitted_chunk_for_raw_summary(raw: &str) -> String {
        let provider = RawTextProvider {
            text: raw.to_string(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TurnEvent>(8);
        run_summary_with_events(&provider, "earlier narration".to_string(), Some(&tx))
            .await
            .expect("graceful summary should succeed");
        let mut chunk_delta = None;
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Chunk { delta } = event {
                chunk_delta = Some(delta);
            }
        }
        chunk_delta.expect("max-iteration exit must emit a TurnEvent::Chunk")
    }

    // The emitted chunk is a live display sink (ACP renders it directly;
    // gateway and RPC forward it unchanged), so the summary must go through
    // the same display-safe contract as the ordinary final response: hidden
    // think content stripped, trailing terminal markers stripped, and an
    // internal tool-protocol envelope suppressed rather than rendered.
    #[tokio::test]
    async fn graceful_summary_chunk_strips_hidden_think_content() {
        let delta =
            emitted_chunk_for_raw_summary("<think>secret chain of thought</think>wrap-up summary")
                .await;
        assert!(
            !delta.contains("secret chain of thought"),
            "hidden think content must not reach the display chunk: {delta}"
        );
        assert!(
            delta.contains("wrap-up summary"),
            "visible summary text must survive: {delta}"
        );
        assert!(
            delta.contains("Turn stopped: reached maximum tool iterations (2)"),
            "chunk must still carry the stop reason: {delta}"
        );
        assert!(
            !delta.contains("earlier narration"),
            "chunk must not re-send earlier narration: {delta}"
        );
    }

    #[tokio::test]
    async fn graceful_summary_chunk_strips_trailing_terminal_marker() {
        let delta = emitted_chunk_for_raw_summary("wrap-up summary<|eom|>").await;
        assert!(
            !delta.contains("eom"),
            "trailing terminal marker must not reach the display chunk: {delta}"
        );
        assert!(
            delta.contains("wrap-up summary"),
            "visible summary text must survive: {delta}"
        );
        assert!(
            delta.contains("Turn stopped: reached maximum tool iterations (2)"),
            "chunk must still carry the stop reason: {delta}"
        );
    }

    #[tokio::test]
    async fn graceful_summary_chunk_suppresses_internal_tool_protocol_envelope() {
        let delta = emitted_chunk_for_raw_summary(
            "<tool_call>{\"name\": \"shell\", \"arguments\": {\"command\": \"ls\"}}</tool_call>",
        )
        .await;
        assert!(
            !delta.contains("tool_call"),
            "internal tool-protocol envelope must not be rendered: {delta}"
        );
        assert!(
            !delta.contains("shell"),
            "internal tool-protocol payload must not be rendered: {delta}"
        );
        assert!(
            delta.contains("internal tool-call format error"),
            "suppressed protocol output must fall back to the safe notice: {delta}"
        );
        assert!(
            delta.contains("Turn stopped: reached maximum tool iterations (2)"),
            "chunk must still carry the stop reason: {delta}"
        );
        assert!(
            !delta.contains("earlier narration"),
            "chunk must not re-send earlier narration: {delta}"
        );
    }
}

#[cfg(test)]
mod loop_break_wrap_up_tests {
    use super::finish_after_loop_break;
    use crate::agent::turn::LoopKnobs;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::model_provider::{ChatRequest, ChatResponse};
    use zeroclaw_config::schema::PacingConfig;
    use zeroclaw_providers::{ChatMessage, ModelProvider};

    struct HappyProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for HappyProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("wrap-up".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                text: Some("here is what got done".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl Attributable for HappyProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "happy-provider"
        }
    }

    struct FailingProvider;

    struct HangingProvider;

    #[async_trait]
    impl ModelProvider for HangingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            std::future::pending().await
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            std::future::pending().await
        }
    }

    impl Attributable for HangingProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "hanging-provider"
        }
    }

    #[async_trait]
    impl ModelProvider for FailingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("provider down")
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            anyhow::bail!("provider down")
        }
    }

    impl Attributable for FailingProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "failing-provider"
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_break(
        provider: &dyn ModelProvider,
        history: &mut Vec<ChatMessage>,
        accumulated: String,
    ) -> anyhow::Result<String> {
        finish_after_loop_break(
            provider,
            history,
            "custom",
            "test-model",
            None,
            &PacingConfig::default(),
            None,
            "tool 'create_lead' has succeeded 9 times this turn",
            accumulated,
            "trace-break-test",
            10,
            &LoopKnobs::default(),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn break_returns_summary_with_stop_note_not_an_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = HappyProvider {
            calls: Arc::clone(&calls),
        };
        let mut history = vec![
            ChatMessage::user("log the lead"),
            ChatMessage::assistant("working on it"),
        ];
        let out = run_break(&provider, &mut history, "partial work".to_string())
            .await
            .expect("loop break must never fail the turn");
        assert!(
            out.contains("here is what got done"),
            "summary missing: {out}"
        );
        assert!(out.contains("partial work"), "partial work lost: {out}");
        assert!(
            out.contains("Stopped early by the loop guard"),
            "stop note missing: {out}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "provider called once");
    }

    #[tokio::test]
    async fn break_with_dead_provider_returns_canned_partial_not_an_error() {
        let provider = FailingProvider;
        let mut history = vec![ChatMessage::user("log the lead")];
        let out = run_break(&provider, &mut history, "partial work".to_string())
            .await
            .expect("dead provider on wrap-up must still not fail the turn");
        assert!(out.contains("partial work"), "partial work lost: {out}");
        assert!(
            out.contains("Stopped early by the loop guard"),
            "stop note missing: {out}"
        );
        assert!(
            out.contains("could not be generated"),
            "honest fallback note missing: {out}"
        );
    }

    /// The break wrap-up must honor step_timeout like the cap path: a hung
    /// provider resolves to the canned partial answer, never hangs the turn.
    /// (Guards the rebase: upstream rewrote this function's summary path.)
    #[tokio::test]
    async fn break_wrap_up_honors_step_timeout() {
        let provider = HangingProvider;
        let mut history = vec![ChatMessage::user("log the lead")];
        let mut pacing = PacingConfig::default();
        pacing.step_timeout_secs = Some(1);
        let out = finish_after_loop_break(
            &provider,
            &mut history,
            "custom",
            "test-model",
            None,
            &pacing,
            None,
            "tool 'x' broke the loop",
            "partial work".to_string(),
            "trace-break-timeout",
            10,
            &LoopKnobs::default(),
            None,
        )
        .await
        .expect("timed-out wrap-up must fall back, not fail");
        assert!(out.contains("partial work"), "partial work lost: {out}");
        assert!(
            out.contains("Stopped early by the loop guard"),
            "stop note missing: {out}"
        );
    }

    /// Provider stub returning caller-supplied raw summary text, so a test
    /// can drive the display-hygiene path (think content, protocol
    /// envelopes, blank text) on the loop-break exit.
    struct RawTextBreakProvider {
        text: String,
    }

    #[async_trait]
    impl ModelProvider for RawTextBreakProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(self.text.clone())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(self.text.clone()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl Attributable for RawTextBreakProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "raw-text-break-provider"
        }
    }

    async fn run_break_with_text(raw: &str, accumulated: &str) -> anyhow::Result<String> {
        let provider = RawTextBreakProvider {
            text: raw.to_string(),
        };
        let mut history = vec![ChatMessage::user("log the lead")];
        run_break(&provider, &mut history, accumulated.to_string()).await
    }

    // Parity with the max-iteration exit (upstream #10026): hidden think
    // content in the wrap-up summary must not reach the user.
    #[tokio::test]
    async fn break_summary_strips_hidden_think_content() {
        let out = run_break_with_text(
            "<think>secret chain of thought</think>here is what got done",
            "partial work",
        )
        .await
        .expect("loop break must never fail the turn");
        assert!(
            !out.contains("secret chain of thought"),
            "hidden think content must not reach the user: {out}"
        );
        assert!(
            out.contains("here is what got done"),
            "visible summary text must survive: {out}"
        );
        assert!(
            out.contains("Stopped early by the loop guard"),
            "stop note missing: {out}"
        );
    }

    // Parity with the max-iteration exit: an internal tool-protocol envelope
    // is suppressed rather than rendered, with the safe notice instead.
    #[tokio::test]
    async fn break_summary_suppresses_internal_tool_protocol_envelope() {
        let out = run_break_with_text(
            "<tool_call>{\"name\": \"shell\", \"arguments\": {\"command\": \"ls\"}}</tool_call>",
            "partial work",
        )
        .await
        .expect("loop break must never fail the turn");
        assert!(
            !out.contains("tool_call"),
            "internal tool-protocol envelope must not be rendered: {out}"
        );
        assert!(
            !out.contains("shell"),
            "internal tool-protocol payload must not be rendered: {out}"
        );
        assert!(
            out.contains("internal tool-call format error"),
            "suppressed protocol output must fall back to the safe notice: {out}"
        );
        assert!(
            out.contains("Stopped early by the loop guard"),
            "stop note missing: {out}"
        );
    }

    // A whitespace-only summary carries nothing for the user: fall back to
    // the canned partial answer instead of appending blank text.
    #[tokio::test]
    async fn break_whitespace_only_summary_falls_back_to_canned_partial() {
        let out = run_break_with_text("   \n  ", "partial work")
            .await
            .expect("loop break must never fail the turn");
        assert!(out.contains("partial work"), "partial work lost: {out}");
        assert!(
            out.contains("could not be generated"),
            "honest fallback note missing: {out}"
        );
    }

    // The wrap-up must leave the same trace footprint as an ordinary
    // iteration: llm_request + llm_response for the summary call and a
    // turn_final_response for the turn's final text — tagged as wrap-up so
    // they never masquerade as a loop iteration. (Without these, traces go
    // dark exactly when the detector fires.)
    #[tokio::test]
    async fn break_wrap_up_leaves_request_response_final_trace() {
        let _writer_guard = zeroclaw_log::__private_test_writer_lock();
        let _hook_guard = zeroclaw_log::__private_test_hook_lock();
        zeroclaw_log::try_install_capture_subscriber();
        let mut log_rx = zeroclaw_log::subscribe_or_install();
        while log_rx.try_recv().is_ok() {}

        let calls = Arc::new(AtomicUsize::new(0));
        let provider = HappyProvider {
            calls: Arc::clone(&calls),
        };
        let mut history = vec![ChatMessage::user("log the lead")];
        finish_after_loop_break(
            &provider,
            &mut history,
            "custom",
            "test-model",
            None,
            &PacingConfig::default(),
            None,
            "tool 'x' broke the loop",
            "partial work".to_string(),
            "trace-break-wrap-up-events",
            10,
            &LoopKnobs::default(),
            None,
        )
        .await
        .expect("loop break must never fail the turn");

        let mut seen = std::collections::HashSet::new();
        let mut final_text = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while seen.len() < 3 && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let step = remaining.min(std::time::Duration::from_millis(50));
            match tokio::time::timeout(step, log_rx.recv()).await {
                Ok(Ok(value)) => {
                    let is_ours = value.get("trace_id").and_then(|v| v.as_str())
                        == Some("trace-break-wrap-up-events");
                    let msg = value
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    // Only the three wrap-up footprint events carry the tag;
                    // older records on the same trace (e.g. the breaker note)
                    // predate it and are ignored here.
                    if !is_ours
                        || !matches!(msg, "llm_request" | "llm_response" | "turn_final_response")
                    {
                        continue;
                    }
                    let is_wrap_up = value
                        .get("attributes")
                        .and_then(|a| a.get("wrap_up"))
                        .and_then(|v| v.as_str())
                        == Some("loop_break");
                    assert!(
                        is_wrap_up,
                        "wrap-up trace record must carry wrap_up=loop_break: {value}"
                    );
                    if msg == "turn_final_response" {
                        final_text = value
                            .get("attributes")
                            .and_then(|a| a.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                    }
                    seen.insert(msg.to_string());
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_elapsed) => {}
            }
        }
        zeroclaw_log::clear_broadcast_hook();

        for expected in ["llm_request", "llm_response", "turn_final_response"] {
            assert!(
                seen.contains(expected),
                "wrap-up must emit {expected}; saw: {seen:?}"
            );
        }
        assert!(
            final_text.contains("Stopped early by the loop guard"),
            "turn_final_response must carry the stop note: {final_text}"
        );
    }

    #[tokio::test]
    async fn break_error_downcasts_to_loop_break() {
        let err: anyhow::Error = crate::agent::turn::results_collect::LoopBreak {
            message: "boom".to_string(),
        }
        .into();
        let b = err
            .downcast_ref::<crate::agent::turn::results_collect::LoopBreak>()
            .expect("turn loop must recognise the typed break");
        assert_eq!(b.message, "boom");
        assert!(err.to_string().contains("aborted by loop detector"));
    }
}

#[cfg(test)]
mod i18n_message_tests {
    /// The graceful max-iteration shutdown must include the iteration count in
    /// the user-visible message so the operator knows why the agent stopped.
    #[test]
    fn max_iterations_message_includes_count() {
        let msg = crate::i18n::get_required_cli_string_with_args(
            "turn-max-iterations-reached",
            &[("max_iterations", "42")],
        );
        assert!(
            msg.contains("42"),
            "message should contain iteration count: {msg}"
        );
        assert!(
            msg.contains("maximum tool iterations"),
            "message should describe the limit: {msg}"
        );
    }
}
