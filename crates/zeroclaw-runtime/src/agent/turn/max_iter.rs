//! The max-iteration exit: when the loop exhausts its iterations, ask the
//! LLM for a tools-free final summary (with step timeout + cancel select)
//! and return it appended to the accumulated display text, or bail.

use super::StreamDelta;
use super::execution::SettledAttemptSummary;
use super::knobs::{LoopKnobs, MaxIterationBehavior};
use super::outcome::ToolLoopCancelled;
use super::redact::scrub_credentials;
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_api::turn_stop::{TurnStop, TurnStopCode};
use zeroclaw_config::schema::{Config, MultimodalConfig, PacingConfig, ResolvedContextLimits};
use zeroclaw_providers::{ChatMessage, ModelProvider};
use zeroclaw_tool_call_parser::{strip_think_tags, strip_trailing_terminal_markers};

#[derive(Clone, Copy, Debug)]
pub(crate) enum CompletionLimit {
    LocalIterations(usize),
    ExecutionTree,
}

impl CompletionLimit {
    fn explanation(self) -> String {
        match self {
            Self::LocalIterations(limit) => {
                format!("Agent exceeded maximum tool iterations ({limit})")
            }
            Self::ExecutionTree => "Agent exhausted the execution-tree iteration budget".into(),
        }
    }

    /// The iteration-cap stop, shared by the `ErrorAtCap` exit and the
    /// graceful-summary failure paths so all of them carry one code and one message.
    fn stop(self) -> TurnStop {
        TurnStop::close_out(TurnStopCode::MaxIterations, self.explanation())
    }
}

/// Project a finished summary call's settled attempts as `TurnEvent::Usage`
/// so the gateway ledger includes the final-summary billed work. Mirrors
/// `emit_rejected_attempt_usage`, but carries each summary's own `accepted`
/// flag (the accepted leaf is `true`).
async fn emit_summary_attempt_usage(
    event_tx: Option<&Sender<TurnEvent>>,
    summaries: &[SettledAttemptSummary],
) {
    if let Some(tx) = event_tx {
        for summary in summaries {
            let _ = tx
                .send(TurnEvent::Usage {
                    input_tokens: summary.input_tokens,
                    cached_input_tokens: summary.cached_input_tokens,
                    output_tokens: summary.output_tokens,
                    cost_usd: summary.cost_usd,
                    context_token_budget: None,
                    model_context_window: None,
                    provider_ref: summary.provider_ref.clone(),
                    model: summary.model.clone(),
                    accepted: summary.accepted,
                    estimated_input_tokens: summary.estimated_input_tokens,
                })
                .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_after_max_iterations(
    injected_memory_preamble: &mut Option<super::MemoryPreamble>,
    model_provider: &dyn ModelProvider,
    history: &mut Vec<ChatMessage>,
    provider_name: &str,
    model: &str,
    dispatch_model: &str,
    temperature: Option<f64>,
    pacing: &PacingConfig,
    cancellation_token: Option<&CancellationToken>,
    limit: CompletionLimit,
    mut accumulated_display_text: String,
    turn_id: &str,
    knobs: &LoopKnobs,
    event_tx: Option<&Sender<TurnEvent>>,
    on_delta: Option<&Sender<StreamDelta>>,
    mut new_messages_out: Option<&mut Vec<ChatMessage>>,
    config: Option<&Config>,
    multimodal_config: &MultimodalConfig,
    security: Option<&crate::security::SecurityPolicy>,
    hooks: Option<&crate::hooks::HookRunner>,
    mut image_cache: Option<&mut zeroclaw_providers::multimodal::LocalImageCache>,
    provider_image_state: Option<&mut super::ProviderImageState>,
    context_limits_for_route: impl Fn(&str, &str) -> ResolvedContextLimits + Send + Sync,
    crumb_present: &mut bool,
    reported_usage: Option<super::ReportedRequestUsage>,
    observer: &dyn crate::observability::Observer,
) -> Result<String> {
    let exhaustion = limit.explanation();
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "model": model,
                "limit": format!("{limit:?}"),
                "trace_id": turn_id,
            })),
        "tool_loop_exhausted"
    );

    // ErrorAtCap callers (embedders driving Agent::turn) treat the cap as a
    // control signal: bail instead of spending another LLM call on a summary.
    if knobs.max_iteration_behavior == MaxIterationBehavior::ErrorAtCap {
        return Err(limit.stop().into());
    }

    // Graceful shutdown: ask the LLM for a final summary without tools
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({"limit": format!("{limit:?}")})),
        "Iteration allowance exhausted, requesting final summary"
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

    // Resolve against the real latest user turn, before the synthetic summary
    // prompt could hide a fresh image from the vision-capability policy.
    let (vision_provider, degrade_strip_images) =
        super::vision_route::resolve_vision_provider_with_allowed_refs(
            config,
            model_provider,
            history,
            multimodal_config,
            provider_name,
            model,
            dispatch_model,
            security,
            knobs.provider_attempt_allowlist.as_deref(),
        )
        .await?;
    let (model_provider, provider_name, model, dispatch_model) = match vision_provider.as_ref() {
        Some(route) => (
            route.provider.as_ref(),
            route.provider_name.as_str(),
            route.model.as_str(),
            route.model.as_str(),
        ),
        None => (model_provider, provider_name, model, dispatch_model),
    };
    let summary_prompt = ChatMessage::user(format!(
        "{exhaustion}. Please provide your best answer based on the work completed so far. \
         Summarize what you accomplished and what remains to be done."
    ));
    let summary_prompt_mirror = summary_prompt.clone();

    enum SummaryCall {
        Cancelled,
        TimedOut(u64),
        Done(Result<zeroclaw_providers::ChatResponse>),
    }
    // Collect the summary call's settled attempts so they can be projected
    // as Usage events below: without this the gateway ledger omits the
    // final-summary billed work. Declared outside the call block so the
    // match arms below can read it after the future completes.
    let mut summary_attempts = Vec::new();
    let summary_call = {
        // Preparation and hooks belong to the same timeout/cancellation scope
        // as dispatch; only a successful summary persists the prompt.
        let summary_future = async {
            // Exclude the synthetic prompt so trailing tool-result images
            // remain in the latest run and receive normal image validation.
            let mut messages = super::vision_route::prepare_messages_for_iteration(
                history,
                multimodal_config,
                degrade_strip_images,
                image_cache.as_deref_mut(),
            )
            .await?
            .messages;
            let prepared_message_count = messages.len();
            messages.push(summary_prompt_mirror.clone());
            let pre_hook_messages = messages.clone();
            let mut selected_model = model.to_string();
            if let Some(hooks) = hooks.filter(|hooks| !hooks.is_empty())
                && let crate::hooks::HookResult::Cancel(reason) = hooks
                    .run_before_llm_call(&mut messages, &mut selected_model)
                    .await
            {
                anyhow::bail!("LLM call cancelled by hook: {reason}");
            }
            let context_limits = context_limits_for_route(provider_name, &selected_model);
            let trim_budget = super::dispatch_trim_budget(context_limits);
            let token_counter = super::DispatchTokenCounter::for_route(
                reported_usage,
                provider_name,
                &selected_model,
            );
            let suffix_only = messages.len() >= pre_hook_messages.len()
                && messages
                    .iter()
                    .zip(&pre_hook_messages)
                    .all(|(a, b)| a.role == b.role && a.content == b.content);
            let hook_suffix = if suffix_only {
                messages[pre_hook_messages.len()..].to_vec()
            } else {
                Vec::new()
            };
            let summary_image_route =
                super::ProviderImageState::route(provider_name, &selected_model);
            let quarantined_image_ids = provider_image_state
                .map(|state| state.quarantined(&summary_image_route))
                .unwrap_or_default();
            if !quarantined_image_ids.is_empty() {
                if !suffix_only {
                    anyhow::bail!(crate::i18n::get_required_cli_string(
                        "turn-context-hook-mutation-unsafe-error",
                    ));
                }
                let transient_suffix = messages.split_off(prepared_message_count);
                messages = super::suppress_quarantined_provider_images(
                    &messages,
                    &quarantined_image_ids,
                    false,
                );
                messages.extend(transient_suffix);
            }
            let tokens_before =
                token_counter.count(crate::agent::history::estimate_history_tokens(&messages));
            let mut dropped_messages = 0;
            let mut dropped_turns = 0;
            loop {
                let tokens =
                    token_counter.count(crate::agent::history::estimate_history_tokens(&messages));
                let mut trim = super::surface_oversized_dispatch_if_needed(
                    injected_memory_preamble,
                    history,
                    crumb_present,
                    tokens,
                    trim_budget,
                );
                dropped_messages += trim.dropped_messages;
                if trim.outcome == super::PreDispatchOutcome::Trimmed {
                    // Rebuild only an unchanged prefix plus a transient suffix.
                    // Arbitrary hook rewrites cannot be replayed without invoking the hook twice.
                    if !suffix_only {
                        anyhow::bail!(crate::i18n::get_required_cli_string(
                            "turn-context-hook-mutation-unsafe-error",
                        ));
                    }
                    dropped_turns += 1;
                    messages = super::vision_route::prepare_messages_for_iteration(
                        history,
                        multimodal_config,
                        degrade_strip_images,
                        image_cache.as_deref_mut(),
                    )
                    .await?
                    .messages;
                    messages = super::suppress_quarantined_provider_images(
                        &messages,
                        &quarantined_image_ids,
                        false,
                    );
                    messages.push(summary_prompt_mirror.clone());
                    messages.extend(hook_suffix.iter().cloned());
                    continue;
                }
                trim.dropped_messages = dropped_messages;
                super::record_dispatch_trim(
                    (provider_name, &selected_model),
                    &trim,
                    dropped_turns,
                    trim_budget,
                    tokens_before,
                    tokens,
                    token_counter.source(),
                );
                let floor = tokens > context_limits.model_context_window as u64;
                let event_budget = if floor {
                    context_limits.model_context_window
                } else {
                    trim_budget
                };
                if dropped_messages > 0 || floor {
                    let source = token_counter.source();
                    let reason = crate::i18n::get_required_cli_string("history-trim-reason-budget");
                    if let Some(tx) = event_tx {
                        let _ = tx
                            .send(TurnEvent::HistoryTrimmed {
                                dropped_messages,
                                dropped_turns,
                                kept_turns: trim.kept_turns,
                                reason: reason.clone(),
                                token_budget: Some(event_budget as u64),
                                tokens_before: Some(tokens_before),
                                tokens_after: Some(tokens),
                                tokens_before_source: Some(source),
                                tokens_after_source: Some(source),
                                unsatisfiable_floor: floor.then_some(true),
                                retained_context: Some(super::retained_context_snapshot(
                                    injected_memory_preamble,
                                    history,
                                    *crumb_present,
                                )),
                            })
                            .await;
                    }
                    observer.record_event(
                        &zeroclaw_api::observability_traits::ObserverEvent::HistoryTrimmed {
                            dropped_messages,
                            kept_turns: trim.kept_turns,
                            reason,
                            channel: None,
                            agent_alias: None,
                            turn_id: None,
                            token_budget: Some(event_budget as u64),
                            tokens_before: Some(tokens_before),
                            tokens_after: Some(tokens),
                            tokens_before_source: Some(source),
                            tokens_after_source: Some(source),
                            unsatisfiable_floor: floor.then_some(true),
                        },
                    );
                }
                if floor {
                    return Err(super::context_window_exceeded_error(
                        (provider_name, &selected_model),
                        context_limits,
                        tokens,
                        token_counter.source(),
                    ));
                }
                break;
            }
            let summary_request = zeroclaw_providers::ChatRequest {
                messages: &messages,
                tools: None, // No tools — force a text response
                thinking: zeroclaw_api::NATIVE_THINKING_OVERRIDE
                    .try_with(Clone::clone)
                    .ok()
                    .flatten(),
            };
            let access = crate::agent::turn::execution::ResolvedModelAccess {
                model_provider,
                provider_name,
                model: &selected_model,
                dispatch_model: if selected_model == model {
                    dispatch_model
                } else {
                    &selected_model
                },
                temperature,
            };
            // Route the graceful-summary call through the metered provider seam. This
            // was the one tool-loop provider call that skipped the budget check and
            // recorded no cost; through the seam it now fails closed when the turn's
            // budget is exhausted and its token usage is charged like any in-loop
            // call. Metering is a no-op when the turn is unscoped.
            access
                .run_model_query(summary_request, &mut summary_attempts)
                .await
        };
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
            return Err(ToolLoopCancelled.into());
        }
        SummaryCall::TimedOut(step_secs) => {
            return Err(TurnStop::close_out(
                TurnStopCode::MaxIterations,
                format!("Final summary LLM call timed out after {step_secs}s (step_timeout_secs)"),
            )
            .into());
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
                        "limit": format!("{limit:?}"),
                        "trace_id": turn_id,
                        "error": format!("{e}"),
                    })),
                "final summary LLM call failed after iteration exhaustion; bailing"
            );
            emit_summary_attempt_usage(event_tx, &summary_attempts).await;
            // The provider error stays the source; the stop rides alongside
            // it so the exit is typed without losing what actually failed.
            return Err(zeroclaw_api::turn_stop::tag(
                e.context(exhaustion),
                limit.stop(),
            ));
        }
        SummaryCall::Done(Ok(resp)) => {
            // When the provider omitted `input_tokens` on the accepted
            // summary, carry a display-only estimate of the summary request
            // so the context meter keeps its numerator after the final-summary
            // event. Rejected attempts and accounting stay provider-reported.
            let accepted_estimate = resp
                .usage
                .as_ref()
                .and_then(|u| u.input_tokens)
                .is_none()
                .then(|| crate::agent::history::estimate_history_tokens(history) as u64)
                .filter(|&est| est > 0);
            if let Some(est) = accepted_estimate {
                for summary in &mut summary_attempts {
                    if summary.accepted {
                        summary.estimated_input_tokens = Some(est);
                    }
                }
            }
            emit_summary_attempt_usage(event_tx, &summary_attempts).await;
            resp
        }
    };

    let raw_text = resp.text.unwrap_or_default();
    if raw_text.is_empty() {
        return Err(limit.stop().into());
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
                    "limit": format!("{limit:?}"),
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
        return Err(limit.stop().into());
    }
    // History and result payloads keep the unmodified provider text; only the
    // display path is normalized, matching the final-response contract.
    history.push(summary_prompt);
    let summary_msg = ChatMessage::assistant(raw_text.clone());
    if let Some(out) = &mut new_messages_out {
        out.push(summary_prompt_mirror);
        out.push(summary_msg.clone());
    }
    history.push(summary_msg);
    // Graceful shutdown with a visible reason so the user knows why the
    // agent stopped making progress.
    let stop_reason = match limit {
        CompletionLimit::LocalIterations(max_iterations) => {
            crate::i18n::get_required_cli_string_with_args(
                "turn-max-iterations-reached",
                &[("max_iterations", &max_iterations.to_string())],
            )
        }
        CompletionLimit::ExecutionTree => {
            crate::i18n::get_required_cli_string("turn-execution-tree-budget-reached")
        }
    };
    let segment = format!("{display_text}\n\n{stop_reason}");
    // This summary is the turn's only visible output on the max-iteration
    // exit path, and it comes from a fresh non-streaming call — there is no
    // live delta a client could have already received, so unlike the normal
    // final-response path this emit needs no live-vs-post-hoc guard. Only the
    // newly-produced segment goes out; `accumulated_display_text` holds
    // narration earlier iterations already streamed, and resending it here
    // would duplicate it in the client.
    super::events::emit_posthoc_turn_chunk(event_tx, &segment).await;
    if let Some(tx) = on_delta {
        let _ = tx.send(StreamDelta::Text(segment.clone())).await;
    }
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
            dispatch_model: model,
            temperature,
        };
        // The wrap-up is a single best-effort call; its rejected-attempt
        // summaries are not reported anywhere.
        let mut settled_attempts = Vec::new();
        let summary_future = access.run_model_query(summary_request, &mut settled_attempts);
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
    use super::{CompletionLimit, finish_after_max_iterations};
    use crate::agent::cost::{TOOL_LOOP_COST_TRACKING_CONTEXT, ToolLoopCostTrackingContext};
    use crate::agent::turn::{LoopKnobs, StreamDelta};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::model_provider::{
        ChatRequest, ChatResponse, ProviderCapabilities, SemanticEmptyTerminalCompletion,
    };
    use zeroclaw_config::schema::{
        CostConfig, MultimodalConfig, PacingConfig, ResolvedContextLimits,
    };
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
                    cache_creation_input_tokens: None,
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

    /// Provider that omits `usage` entirely, like llama.cpp and other
    /// local OpenAI-compatible servers.
    struct UsageOmittingProvider;

    #[async_trait]
    impl ModelProvider for UsageOmittingProvider {
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
            Ok(ChatResponse {
                text: Some("wrap-up summary".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl Attributable for UsageOmittingProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "usage-omitting-provider"
        }
    }

    async fn run_summary_with_events(
        provider: &dyn ModelProvider,
        accumulated_display_text: String,
        event_tx: Option<&Sender<TurnEvent>>,
        on_delta: Option<&Sender<StreamDelta>>,
    ) -> anyhow::Result<String> {
        let mut history = vec![ChatMessage::user("do the work")];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default(); // GracefulSummary
        let multimodal_config = MultimodalConfig::default();
        finish_after_max_iterations(
            &mut None,
            provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            accumulated_display_text,
            "trace-req-test",
            &knobs,
            event_tx,
            on_delta,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
    }

    async fn run_summary(provider: &dyn ModelProvider) -> anyhow::Result<String> {
        run_summary_with_events(provider, String::new(), None, None).await
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

    // The final-summary call's billed work must reach the event ledger, not
    // just the accumulator: the gateway derives done.cost_usd from emitted
    // Usage events, so an unprojected summary silently under-reports the turn.
    #[tokio::test]
    async fn graceful_summary_projects_usage_event_for_gateway_ledger() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingUsageProvider {
            calls: Arc::clone(&calls),
        };
        let ctx = ToolLoopCostTrackingContext::usage_only();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);

        let out = TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(
                Some(ctx),
                run_summary_with_events(&provider, String::new(), Some(&event_tx), None),
            )
            .await
            .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let mut usage_events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let TurnEvent::Usage {
                input_tokens,
                output_tokens,
                accepted,
                ..
            } = event
            {
                usage_events.push((input_tokens, output_tokens, accepted));
            }
        }
        // Exactly the summary call's accepted attempt — no more, no less.
        assert_eq!(usage_events, vec![(Some(100), Some(20), true)]);
    }

    // The graceful summary now fails closed on budget exhaustion: it was the one
    // tool-loop provider call that skipped the budget check. A tripped budget
    // (negative limit) makes the seam bail BEFORE spending, so the provider is
    // never called and the cap is surfaced as an error.
    #[tokio::test]
    async fn graceful_summary_carries_estimate_when_provider_omits_input_tokens() {
        let provider = UsageOmittingProvider;
        let ctx = ToolLoopCostTrackingContext::usage_only();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);

        let out = TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(
                Some(ctx),
                run_summary_with_events(&provider, String::new(), Some(&event_tx), None),
            )
            .await
            .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let mut usage_events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let TurnEvent::Usage {
                input_tokens,
                estimated_input_tokens,
                accepted,
                ..
            } = event
            {
                usage_events.push((input_tokens, estimated_input_tokens, accepted));
            }
        }
        // The provider omitted input_tokens; the accepted summary carries a
        // display-only estimate, not a measured count.
        assert_eq!(usage_events.len(), 1, "expected exactly one usage event");
        let (input_tokens, estimated, accepted) = usage_events[0];
        assert_eq!(input_tokens, None, "measured input_tokens must stay None");
        assert!(
            estimated.is_some_and(|est| est > 0),
            "accepted summary must carry a positive estimate, got {estimated:?}"
        );
        assert!(accepted, "summary attempt must be accepted");
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
                    cache_creation_input_tokens: None,
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
    /// test can assert on what actually reached the provider. `vision` lets a
    /// test decide whether the provider can see images, which the summary
    /// path consults exactly like the in-loop path.
    #[derive(Default)]
    struct CapturingProvider {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        vision: bool,
        expected_request: Option<(&'static str, usize)>,
    }

    #[async_trait]
    impl ModelProvider for CapturingProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                vision: self.vision,
                ..Default::default()
            }
        }

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
            model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            if let Some((expected_model, token_budget)) = self.expected_request {
                assert_eq!(model, expected_model);
                assert!(request.tools.is_none());
                assert!(
                    crate::agent::history::estimate_history_tokens(request.messages)
                        <= token_budget
                );
            }
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

    #[tokio::test]
    async fn graceful_summary_counts_hook_selected_request_without_repeating_hook() {
        struct SummaryHook(Arc<AtomicUsize>);
        #[async_trait]
        impl crate::hooks::HookHandler for SummaryHook {
            fn name(&self) -> &str {
                "summary-context-hook"
            }
            async fn before_llm_call(
                &self,
                messages: &mut Vec<ChatMessage>,
                model: &mut String,
            ) -> crate::hooks::HookResult<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                *model = "summary-model".into();
                messages.push(ChatMessage::user("hook context ".repeat(80)));
                crate::hooks::HookResult::Continue(())
            }
        }

        for budget in [512, 1] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let provider = CapturingProvider {
                seen: Arc::clone(&seen),
                expected_request: Some(("summary-model", budget)),
                ..Default::default()
            };
            let calls = Arc::new(AtomicUsize::new(0));
            let mut hooks = crate::hooks::HookRunner::new();
            hooks.register(Box::new(SummaryHook(Arc::clone(&calls))));
            let mut history = vec![
                ChatMessage::user("obsolete context ".repeat(800)),
                ChatMessage::assistant("obsolete answer"),
                ChatMessage::user("latest real question"),
                ChatMessage::assistant("latest completed work"),
            ];
            let mut crumb_present = false;
            let result = finish_after_max_iterations(
                &mut None,
                &provider,
                &mut history,
                "custom",
                "original-model",
                "hint:original-model",
                None,
                &PacingConfig::default(),
                None,
                CompletionLimit::ExecutionTree,
                String::new(),
                "summary-hook-context",
                &LoopKnobs::default(),
                None,
                None,
                None,
                None,
                &MultimodalConfig::default(),
                None,
                Some(&hooks),
                None,
                None,
                |provider, model| {
                    assert_eq!(provider, "custom");
                    assert_eq!(model, "summary-model");
                    ResolvedContextLimits {
                        model_context_window: budget,
                        ..ResolvedContextLimits::legacy_fallback(budget)
                    }
                },
                &mut crumb_present,
                None,
                &crate::observability::NoopObserver,
            )
            .await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(crumb_present);
            assert!(history.iter().any(|m| m.content == "latest real question"));
            assert!(history.iter().any(|m| m.content == "latest completed work"));
            assert!(!history.iter().any(|m| m.content.contains("obsolete")));
            assert!(!history.iter().any(|m| m.content.contains("hook context")));
            let captured = seen.lock().unwrap();
            if budget == 512 {
                assert!(result.unwrap().contains("execution-tree iteration budget"));
                assert_eq!(captured.len(), 1);
                assert!(captured[0].contains("latest real question"));
                assert!(captured[0].contains("hook context"));
                assert!(!captured[0].contains("obsolete"));
            } else {
                let error = result.unwrap_err();
                let exceeded = super::super::context_window_exceeded_from_error(&error)
                    .expect("summary capacity rejection must retain its typed cause");
                assert_eq!(exceeded.model_context_window, budget);
                assert!(exceeded.estimated_tokens > budget);
                assert!(captured.is_empty());
                assert!(
                    !history
                        .iter()
                        .any(|m| m.content.contains("execution-tree iteration budget"))
                );
            }
        }
    }

    #[tokio::test]
    async fn graceful_summary_omits_quarantined_paired_tool_image_before_and_after_trim() {
        struct SummaryRouteHook(Arc<AtomicUsize>);

        #[async_trait]
        impl crate::hooks::HookHandler for SummaryRouteHook {
            fn name(&self) -> &str {
                "summary-route-hook"
            }

            async fn before_llm_call(
                &self,
                messages: &mut Vec<ChatMessage>,
                model: &mut String,
            ) -> crate::hooks::HookResult<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                *model = "summary-model".into();
                messages.push(ChatMessage::user("hook context ".repeat(20)));
                crate::hooks::HookResult::Continue(())
            }
        }

        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let image_reference = format!("data:image/png;base64,{PNG_B64}");
        let multimodal_config = MultimodalConfig::default();

        for budget in [4096, 512] {
            let assistant_content = super::super::parse_response::build_native_assistant_history(
                "",
                &[zeroclaw_api::model_provider::ToolCall {
                    id: "toolu_image".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                    extra_content: None,
                }],
                None,
            );
            let mut history = vec![
                ChatMessage::user("obsolete context ".repeat(800)),
                ChatMessage::assistant("obsolete answer"),
                ChatMessage::user("capture the screenshot"),
                ChatMessage::assistant(assistant_content),
                ChatMessage::tool(
                    serde_json::json!({
                        "content": "captured screenshot",
                        "tool_call_id": "toolu_image",
                        "attachments": [{"kind": "image", "target": image_reference}],
                    })
                    .to_string(),
                ),
            ];
            let prepared = super::super::vision_route::prepare_messages_for_iteration(
                &history,
                &multimodal_config,
                false,
                None,
            )
            .await
            .expect("tool image prepares");
            let submitted = zeroclaw_providers::multimodal::provider_image_ids(&prepared.messages);
            assert_eq!(
                submitted.len(),
                1,
                "the paired tool image must be provider-visible"
            );

            let mut provider_image_state = super::super::ProviderImageState::default();
            provider_image_state.record_recovery(
                super::super::ProviderImageState::route("custom", "summary-model"),
                &[],
                &submitted,
            );
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let provider = CapturingProvider {
                seen: Arc::clone(&seen),
                vision: true,
                expected_request: Some(("summary-model", budget)),
            };
            let hook_calls = Arc::new(AtomicUsize::new(0));
            let mut hooks = crate::hooks::HookRunner::new();
            hooks.register(Box::new(SummaryRouteHook(Arc::clone(&hook_calls))));
            let mut crumb_present = false;
            let mut memory_preamble = None;

            let out = finish_after_max_iterations(
                &mut memory_preamble,
                &provider,
                &mut history,
                "custom",
                "original-model",
                "original-model",
                None,
                &PacingConfig::default(),
                None,
                CompletionLimit::LocalIterations(2),
                String::new(),
                "summary-quarantined-tool-image",
                &LoopKnobs::default(),
                None,
                None,
                None,
                None,
                &multimodal_config,
                None,
                Some(&hooks),
                None,
                Some(&mut provider_image_state),
                |provider, model| {
                    assert_eq!(provider, "custom");
                    assert_eq!(model, "summary-model");
                    ResolvedContextLimits {
                        model_context_window: budget,
                        ..ResolvedContextLimits::legacy_fallback(budget)
                    }
                },
                &mut crumb_present,
                None,
                &crate::observability::NoopObserver,
            )
            .await
            .expect("graceful summary succeeds");

            assert!(out.contains("wrap-up summary"));
            assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
            let captured = seen.lock().unwrap();
            assert!(captured[0].contains("hook context"));
            assert!(
                !captured[0].contains(PNG_B64),
                "the quarantined paired tool image reached the summary provider: {}",
                captured[0]
            );
            assert_eq!(crumb_present, budget == 512);
            assert_eq!(
                history
                    .iter()
                    .any(|message| message.content.contains("obsolete")),
                budget == 4096
            );
        }
    }

    #[tokio::test]
    async fn graceful_summary_prompt_can_make_the_latest_turn_unsatisfiable() {
        for disable_soft_budget in [false, true] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let provider = CapturingProvider {
                seen: Arc::clone(&seen),
                vision: false,
                ..Default::default()
            };
            let mut history = vec![
                ChatMessage::user("current question"),
                ChatMessage::assistant("work completed"),
            ];
            // The real turn fits exactly; only the synthetic prompt can exceed it.
            let budget = crate::agent::history::estimate_history_tokens(&history);
            let mut crumb_present = false;
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let error = finish_after_max_iterations(
                &mut None,
                &provider,
                &mut history,
                "custom",
                "test-model",
                "test-model",
                None,
                &PacingConfig::default(),
                None,
                CompletionLimit::LocalIterations(1),
                String::new(),
                "summary-prompt-floor",
                &LoopKnobs::default(),
                Some(&tx),
                None,
                None,
                None,
                &MultimodalConfig::default(),
                None,
                None,
                None,
                None,
                |_, _| ResolvedContextLimits {
                    model_context_window: budget,
                    ..ResolvedContextLimits::legacy_fallback(if disable_soft_budget {
                        0
                    } else {
                        budget
                    })
                },
                &mut crumb_present,
                None,
                &crate::observability::NoopObserver,
            )
            .await
            .expect_err("summary prompt must be included in the floor decision");
            let exceeded = super::super::context_window_exceeded_from_error(&error)
                .expect("summary capacity rejection must retain its typed cause");
            assert_eq!(exceeded.model_context_window, budget);
            assert!(exceeded.estimated_tokens > budget);
            assert!(
                seen.lock().unwrap().is_empty(),
                "no oversized summary dispatch"
            );
            assert_eq!(history.len(), 2);
            assert_eq!(history[0].content, "current question");
            assert_eq!(history[1].content, "work completed");
            assert!(!crumb_present);
            let TurnEvent::HistoryTrimmed {
                tokens_after,
                unsatisfiable_floor,
                retained_context,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("expected summary floor event");
            };
            assert_eq!(unsatisfiable_floor, Some(true));
            assert!(tokens_after.unwrap() > budget as u64);
            let retained = retained_context.expect("summary floor must preserve retained context");
            assert!(!retained.breadcrumb);
            assert_eq!(retained.retained_messages.len(), 2);
        }
    }

    // The summary request must drop declared audio attachments before dispatch,
    // just like an in-loop request. Legacy tool text is not scanned for markers.
    #[tokio::test]
    async fn graceful_summary_drops_declared_tool_audio_attachment() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: false,
            ..Default::default()
        };
        let attachments = vec![zeroclaw_api::media::RenderedMarker {
            target: "/tmp/clip.wav".to_string(),
            kind: zeroclaw_api::media::MarkerKind::Audio,
        }];
        let tool_content = serde_json::json!({
            "content": "recorded 3:00 PM",
            "tool_call_id": "toolu_1",
            "attachments": zeroclaw_api::tool_carrier::render_native_attachments(&attachments),
        })
        .to_string();
        let mut history = vec![
            ChatMessage::user("call the tool and tell me what you hear"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_1"}]}"#),
            ChatMessage::tool(tool_content),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-audio",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            !captured.contains("/tmp/clip.wav"),
            "declared audio path reached the provider on the max-iteration path: {captured}"
        );
        assert!(
            captured.contains("recorded 3:00 PM"),
            "tool text should survive dropping the audio attachment: {captured}"
        );
    }

    // The summary request is prepared like an in-loop request, so an image a
    // tool DECLARED whose file is not an interpretable image is dropped by
    // the normalizer with a model-facing note: the raw path never reaches
    // the provider, and the model is told the attachment could not be loaded
    // rather than being handed a path to hallucinate over.
    #[tokio::test]
    async fn graceful_summary_drops_unloadable_tool_image_marker() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bogus_image = temp.path().join("shot.txt");
        std::fs::write(&bogus_image, b"not an image").expect("write text bytes");
        let attachments = vec![zeroclaw_api::media::RenderedMarker {
            target: bogus_image.display().to_string(),
            kind: zeroclaw_api::media::MarkerKind::Image,
        }];

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        // A properly paired assistant tool_call + declared native tool-result
        // envelope, so the orphan sweep keeps the exchange intact and the
        // declared attachment reaches the summary preparation as a
        // latest-run tool result.
        let tool_content = serde_json::json!({
            "content": "shot",
            "tool_call_id": "toolu_img",
            "attachments": zeroclaw_api::tool_carrier::render_native_attachments(&attachments),
        })
        .to_string();
        let mut history = vec![
            ChatMessage::user("call the tool and describe the screenshot"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_img"}]}"#),
            ChatMessage::tool(tool_content),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-img-drop",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            !captured.contains(&bogus_image.display().to_string()),
            "raw image path reached the provider on the max-iteration path: {captured}"
        );
        assert!(
            !captured.contains("data:image"),
            "the unloadable declared attachment must yield no image: {captured}"
        );
        assert!(
            captured.contains("could not be loaded"),
            "the normalizer's skipped-image note must reach the model: {captured}"
        );
    }

    // A loadable local image and an already-inline data URI, both DECLARED
    // as attachments, reach the summary request as validated inline data
    // URIs: the file is read, MIME-checked and inlined by the normalizer,
    // and the inline attachment passes through byte-identical, so both
    // arrive as the same payload. This pins the normalize decision for the
    // summary path: the summary may see the turn's images.
    #[tokio::test]
    async fn graceful_summary_normalizes_local_and_inline_tool_image_markers() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let temp = tempfile::tempdir().expect("temp dir");
        let png_path = temp.path().join("shot.png");
        std::fs::write(
            &png_path,
            STANDARD.decode(PNG_B64).expect("valid PNG fixture"),
        )
        .expect("write PNG fixture");
        let inline_uri = format!("data:image/png;base64,{PNG_B64}");
        let attachments = vec![
            zeroclaw_api::media::RenderedMarker {
                target: png_path.display().to_string(),
                kind: zeroclaw_api::media::MarkerKind::Image,
            },
            zeroclaw_api::media::RenderedMarker {
                target: inline_uri.to_string(),
                kind: zeroclaw_api::media::MarkerKind::Image,
            },
        ];

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        let tool_content = serde_json::json!({
            "content": "both shots",
            "tool_call_id": "toolu_two",
            "attachments": zeroclaw_api::tool_carrier::render_native_attachments(&attachments),
        })
        .to_string();
        let mut history = vec![
            ChatMessage::user("call the tool and describe both screenshots"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_two"}]}"#),
            ChatMessage::tool(tool_content),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-img-inline",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            !captured.contains(&png_path.display().to_string()),
            "the local path must be inlined, not forwarded: {captured}"
        );
        assert!(
            captured.contains("both shots"),
            "the declared body is delivered verbatim: {captured}"
        );
        assert_eq!(
            captured.matches(&inline_uri).count(),
            2,
            "both declared images must arrive inlined as the same payload: {captured}"
        );
    }

    // Legacy control: a tool result whose body embeds image marker syntax
    // with no declaration is never promoted, even when the referenced file
    // is a valid image. Nothing is loaded, no note is appended, and the body
    // is delivered as the legacy path delivers it: the one-shot seam's
    // compatibility sweep replaces the path reference with a placeholder.
    #[tokio::test]
    async fn graceful_summary_does_not_promote_legacy_tool_body_markers() {
        let temp = tempfile::tempdir().expect("temp dir");
        let png_path = temp.path().join("shot.png");
        std::fs::write(&png_path, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
            .expect("write png signature");
        let marker = format!("[{}:{}]", "IMAGE", png_path.display());

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        let tool_content = serde_json::json!({
            "content": format!("saw {marker} in output"),
            "tool_call_id": "toolu_legacy",
        })
        .to_string();
        let mut history = vec![
            ChatMessage::user("call the tool and describe the screenshot"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_legacy"}]}"#),
            ChatMessage::tool(tool_content.clone()),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-img-legacy",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            !captured.contains("data:image"),
            "a legacy body marker must not be promoted to an image: {captured}"
        );
        assert!(
            !captured.contains("could not be loaded"),
            "nothing is loaded for a legacy body, so no note is appended: {captured}"
        );
        // A legacy carrier is delivered as its bytes: the body reaches the
        // provider verbatim, quoted marker syntax and raw path included, as
        // text. Nothing infers, sweeps, or strips it. Compare the whole
        // carrier: its JSON escaping doubles a Windows path's backslashes, so
        // the unescaped body never appears in it there.
        assert!(
            captured.contains(&tool_content),
            "the legacy body is delivered verbatim: {captured}"
        );
        assert!(
            !captured.contains(zeroclaw_providers::multimodal::MEDIA_PLACEHOLDER),
            "no seam rewrites a legacy carrier's body: {captured}"
        );
    }

    // A declared count-zero tool result whose body quotes a path marker: the
    // body is text under the attachment-identity contract, and the one-shot
    // dispatch seam behind the graceful summary must deliver it byte for
    // byte instead of replacing the quoted reference with a placeholder.
    // Both carrier shapes are covered: the native envelope and the prompt
    // carrier.
    #[tokio::test]
    async fn graceful_summary_keeps_declared_zero_tool_body_verbatim() {
        let marker = format!("[{}:{}]", "IMAGE", "/tmp/example.png");
        let body = format!("source: {marker} end");

        // Native shape: a properly paired assistant tool_call plus a
        // declared envelope with an empty attachments array, so the orphan
        // sweep keeps the exchange intact.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        let native_content = serde_json::json!({
            "content": body,
            "tool_call_id": "toolu_quote",
            "attachments": zeroclaw_api::tool_carrier::render_native_attachments(&[]),
        })
        .to_string();
        let mut history = vec![
            ChatMessage::user("call the tool and read the file"),
            ChatMessage::assistant(r#"{"tool_calls":[{"id":"toolu_quote"}]}"#),
            ChatMessage::tool(native_content.clone()),
        ];
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-img-quote",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            captured.contains(&native_content),
            "the declared count-zero native body must arrive byte for byte: {captured}"
        );
        assert!(
            !captured.contains(zeroclaw_providers::multimodal::MEDIA_PLACEHOLDER),
            "no placeholder may be substituted at the seam: {captured}"
        );

        // Prompt shape: a count-zero prompt carrier in a user-role message.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        let carrier = zeroclaw_api::tool_carrier::render_prompt_tool_carrier(&body, &[]);
        let mut history = vec![
            ChatMessage::user("call the tool and read the file"),
            ChatMessage::user(carrier.clone()),
        ];
        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-img-quote-prompt",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            captured.contains(&carrier),
            "the declared count-zero prompt carrier must arrive byte for byte: {captured}"
        );
        assert!(
            !captured.contains(zeroclaw_providers::multimodal::MEDIA_PLACEHOLDER),
            "no placeholder may be substituted at the seam: {captured}"
        );
    }

    // The graceful summary replays history through the metered one-shot seam,
    // whose sanitizers used to rewrite each message as a whole string — so a
    // marker inside an assistant envelope's signed thinking text was replaced
    // while its signature stayed, which Anthropic rejects on replay. The
    // envelope (signed thinking included) must reach the provider
    // byte-for-byte, so the assistant entry is built with the production
    // `build_native_assistant_history` builder the adapters parse back.
    #[tokio::test]
    async fn graceful_summary_replays_signed_thinking_unmodified() {
        let marker = format!("[{}:{}]", "IMAGE", "/tmp/shot.png");
        let reasoning =
            format!(r#"{{"thinking":"check {marker} before answering","signature":"sig_replay"}}"#);
        let assistant_content = super::super::parse_response::build_native_assistant_history(
            "",
            &[zeroclaw_api::model_provider::ToolCall {
                id: "toolu_think".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            Some(&reasoning),
        );
        let mut history = vec![
            ChatMessage::user("run the tool"),
            ChatMessage::assistant(assistant_content.clone()),
            ChatMessage::tool(
                serde_json::json!({
                    "content": "done",
                    "tool_call_id": "toolu_think",
                })
                .to_string(),
            ),
        ];
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: true,
            ..Default::default()
        };
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-signed-thinking",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            captured.contains(&assistant_content),
            "the assistant envelope (signed thinking included) must reach the provider byte-for-byte: {captured}"
        );
        assert!(
            !captured.contains(zeroclaw_providers::multimodal::MEDIA_PLACEHOLDER),
            "nothing in this history is a deliverable marker outside the thinking, so a placeholder anywhere means the reasoning was touched: {captured}"
        );
    }

    // Degrade twin of the signed-thinking replay: `vision: false` is what a
    // reliable wrapper reports whenever any fallback lacks vision — even
    // when the primary that receives the request verifies thinking
    // signatures — so the summary's text-only degrade path must keep the
    // envelope byte-identical too, not just the one-shot seam. The history
    // carries a path-form marker in an earlier user turn so the summary
    // actually takes the carried-over-image degrade branch; the placeholder in
    // the captured request is the probe that it ran.
    #[tokio::test]
    async fn graceful_summary_degrade_replays_signed_thinking_unmodified() {
        let user_marker = format!("[{}:{}]", "IMAGE", "/tmp/degrade.png");
        let marker = format!("[{}:{}]", "IMAGE", "/tmp/shot.png");
        let reasoning =
            format!(r#"{{"thinking":"check {marker} before answering","signature":"sig_replay"}}"#);
        let assistant_content = super::super::parse_response::build_native_assistant_history(
            "",
            &[zeroclaw_api::model_provider::ToolCall {
                id: "toolu_think".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            Some(&reasoning),
        );
        let mut history = vec![
            ChatMessage::user(format!("run the tool on {user_marker}")),
            ChatMessage::assistant(assistant_content.clone()),
            ChatMessage::tool(
                serde_json::json!({
                    "content": "done",
                    "tool_call_id": "toolu_think",
                })
                .to_string(),
            ),
            ChatMessage::user("continue without images"),
        ];
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            seen: Arc::clone(&seen),
            vision: false,
            ..Default::default()
        };
        let pacing = PacingConfig::default();
        let knobs = LoopKnobs::default();
        let multimodal_config = MultimodalConfig::default();

        let out = finish_after_max_iterations(
            &mut None,
            &provider,
            &mut history,
            "custom",
            "test-model",
            "test-model",
            None,
            &pacing,
            None,
            CompletionLimit::LocalIterations(2),
            String::new(),
            "trace-req-signed-thinking-degrade",
            &knobs,
            None,
            None,
            None,
            None,
            &multimodal_config,
            None,
            None,
            None,
            None,
            |_, _| ResolvedContextLimits::legacy_fallback(0),
            &mut false,
            None,
            &crate::observability::NoopObserver,
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");
        let captured = seen.lock().unwrap().join("\n");
        assert!(
            captured.contains(&assistant_content),
            "the assistant envelope (signed thinking included) must survive the degrade path byte-for-byte: {captured}"
        );
        assert!(
            captured.contains(zeroclaw_providers::multimodal::MEDIA_PLACEHOLDER),
            "the user-turn marker must be replaced, proving the degrade branch ran: {captured}"
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
            vision: false,
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TurnEvent>(8);

        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<StreamDelta>(8);
        let out = run_summary_with_events(
            &provider,
            "earlier narration".to_string(),
            Some(&tx),
            Some(&delta_tx),
        )
        .await
        .expect("graceful summary should succeed");

        assert!(out.contains("wrap-up summary"), "unexpected summary: {out}");

        let mut chunk_deltas = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Chunk { delta } = event {
                chunk_deltas.push(delta);
            }
        }
        assert_eq!(
            chunk_deltas.len(),
            1,
            "max-iteration exit must emit exactly one TurnEvent::Chunk"
        );
        let delta = chunk_deltas
            .pop()
            .expect("max-iteration exit must emit a TurnEvent::Chunk");
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
        assert!(matches!(
            delta_rx.recv().await,
            Some(StreamDelta::Text(draft_delta)) if draft_delta == delta
        ));
        assert!(
            delta_rx.try_recv().is_err(),
            "summary must emit one draft delta"
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
        run_summary_with_events(&provider, "earlier narration".to_string(), Some(&tx), None)
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

    // A summary that is only a terminal marker passes the semantic-empty
    // check (which strips think tags, not markers) and is emptied by display
    // cleanup; that exit must carry the same typed stop as the other cap exits.
    #[tokio::test]
    async fn graceful_summary_of_only_a_terminal_marker_is_a_typed_max_iterations_stop() {
        let provider = RawTextProvider {
            text: "<|eom|>".to_string(),
        };
        let error = run_summary(&provider)
            .await
            .expect_err("a marker-only summary is not a terminal answer");
        assert_eq!(
            zeroclaw_api::turn_stop::turn_stop(&error)
                .expect("marker-only exit must carry the typed stop")
                .code,
            zeroclaw_api::turn_stop::TurnStopCode::MaxIterations
        );
        assert_eq!(
            error.to_string(),
            "Agent exceeded maximum tool iterations (2)"
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
    async fn loop_detector_stop_is_recognised_for_graceful_wrap_up() {
        let err: anyhow::Error = zeroclaw_api::turn_stop::TurnStop::close_out(
            zeroclaw_api::turn_stop::TurnStopCode::LoopDetector,
            "Agent loop aborted by loop detector: boom",
        )
        .into();
        let message = crate::agent::turn::results_collect::loop_break_message(&err)
            .expect("turn loop must recognise the typed break");
        assert!(message.contains("boom"));
        assert!(
            crate::agent::turn::results_collect::loop_break_message(&anyhow::Error::msg("500"))
                .is_none()
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::turn_stop::turn_stop;

    #[test]
    fn the_iteration_cap_stop_is_typed_and_says_what_it_always_said() {
        let stop = CompletionLimit::LocalIterations(10).stop();
        assert_eq!(stop.code, TurnStopCode::MaxIterations);
        assert_eq!(
            stop.to_string(),
            "Agent exceeded maximum tool iterations (10)"
        );
        let err: anyhow::Error = stop.into();
        assert_eq!(
            turn_stop(&err)
                .expect("stop must survive the anyhow hop")
                .code,
            TurnStopCode::MaxIterations
        );

        let tree = CompletionLimit::ExecutionTree.stop();
        assert_eq!(tree.code, TurnStopCode::MaxIterations);
        assert_eq!(
            tree.to_string(),
            "Agent exhausted the execution-tree iteration budget"
        );
    }
}
