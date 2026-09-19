//! Cross-turn circuit breaker for tools whose SERVER is down.
//!
//! The in-turn detector resets every turn, so a broken MCP server was rediscovered
//! by every new turn: in production one tool failed 102 of 102 calls across 37
//! turns. After `tool_breaker_threshold` consecutive SERVER-side failures of the
//! same tool for the same tenant, further calls are short-circuited with a
//! "temporarily unavailable, do not retry" result instead of hitting the dead
//! server, until a cooldown passes and one probe call is let through.
//!
//! Only server failures count -- the MCP server erroring, timing out or refusing
//! the connection (`MCP server \`x\` ...`). A tool that ANSWERS with an error
//! ("thread not found") proves the server is up and resets the count, so a model
//! passing bad ids can never trip it. Process-global and bounded, like the write
//! velocity gate (single-instance fleet); a restart clears it, which fails toward
//! availability.

use super::context::TurnCtx;
use crate::agent::tool_execution::ToolExecutionOutcome;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Prefix `zeroclaw_tools::mcp_client::dispatch_rpc` puts on every server-side
/// failure (`MCP server \`x\` error during / timed out / failed during ...`).
/// A tool answering `isError`, or a JSON-RPC error, uses different wording.
const SERVER_FAILURE_MARKER: &str = "MCP server `";

/// Marker in the short-circuit result; such results are never recorded back.
pub(crate) const UNAVAILABLE_MARKER: &str = "is temporarily unavailable";

const MAX_KEYS: usize = 10_000;
const MAX_COOLDOWN: Duration = Duration::from_secs(300);
const LAST_ERROR_CHARS: usize = 160;

#[derive(Debug, Default)]
struct State {
    consecutive: usize,
    open_until: Option<Instant>,
    last_error: String,
}

/// What a blocked call learns.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Blocked {
    pub(crate) failures: usize,
    pub(crate) retry_in_secs: u64,
    pub(crate) last_error: String,
}

#[derive(Debug, Default)]
pub(crate) struct BreakerRegistry {
    map: HashMap<(String, String), State>,
}

impl BreakerRegistry {
    /// `Some` while the breaker for `key` is open. Once the cooldown has passed
    /// this returns `None` (half-open): the next call is the probe.
    pub(crate) fn check(&self, key: &(String, String), now: Instant) -> Option<Blocked> {
        let state = self.map.get(key)?;
        let until = state.open_until?;
        (until > now).then(|| Blocked {
            failures: state.consecutive,
            retry_in_secs: until.saturating_duration_since(now).as_secs().max(1),
            last_error: state.last_error.clone(),
        })
    }

    /// Fold one executed call's outcome into the breaker.
    pub(crate) fn record(
        &mut self,
        key: (String, String),
        now: Instant,
        success: bool,
        server_failure: bool,
        error: &str,
        threshold: usize,
        cooldown: Duration,
    ) {
        if threshold == 0 {
            return;
        }
        if success || !server_failure {
            // The server answered (even with an error): it is alive.
            self.map.remove(&key);
            return;
        }
        if self.map.len() >= MAX_KEYS && !self.map.contains_key(&key) {
            self.map.clear(); // bounded; fails toward availability
        }
        let state = self.map.entry(key).or_default();
        state.consecutive += 1;
        state.last_error = error.chars().take(LAST_ERROR_CHARS).collect();
        if state.consecutive >= threshold {
            // First open = cooldown; each failed probe doubles it, capped.
            let doublings = u32::try_from(state.consecutive - threshold)
                .unwrap_or(u32::MAX)
                .min(8);
            let wait = cooldown.saturating_mul(1u32 << doublings).min(MAX_COOLDOWN);
            state.open_until = Some(now + wait);
        }
    }
}

static REGISTRY: LazyLock<Mutex<BreakerRegistry>> =
    LazyLock::new(|| Mutex::new(BreakerRegistry::default()));

fn key_for(tool_name: &str) -> (String, String) {
    let tenant = crate::agent::tenant::current_tenant()
        .map(|t| t.tenant_id.clone())
        .unwrap_or_default();
    (tenant, tool_name.to_string())
}

fn applies_to(tool_name: &str) -> bool {
    // MCP and skill tools only; built-ins have no remote server to be down.
    tool_name.contains("__")
}

/// Is this failure the SERVER's fault (as opposed to a tool answering "no")?
pub(crate) fn is_server_failure(message: &str) -> bool {
    message.contains(SERVER_FAILURE_MARKER)
}

/// The short-circuit result for `tool_name`, if its breaker is open.
pub(crate) fn check_open(ctx: &TurnCtx<'_>, tool_name: &str) -> Option<ToolExecutionOutcome> {
    if ctx.pacing.tool_breaker_threshold == 0 || !applies_to(tool_name) {
        return None;
    }
    let blocked = REGISTRY
        .lock()
        .ok()?
        .check(&key_for(tool_name), Instant::now())?;
    let message = format!(
        "Tool `{tool_name}` {UNAVAILABLE_MARKER}: it failed {} times in a row with server errors \
         (last: {}). Do not retry it now -- tell the user it is down, or use another route. It \
         will be tried again automatically in about {}s.",
        blocked.failures, blocked.last_error, blocked.retry_in_secs
    );
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
            .with_category(::zeroclaw_log::EventCategory::Tool)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "tool": tool_name,
                "failures": blocked.failures,
                "retry_in_secs": blocked.retry_in_secs,
            })),
        "tool circuit breaker open: call short-circuited"
    );
    Some(ToolExecutionOutcome {
        output: message.clone(),
        success: false,
        error_reason: Some(message),
        duration: Duration::ZERO,
        receipt: None,
        output_data: None,
    })
}

/// Record an EXECUTED call's outcome (never a short-circuited one).
pub(crate) fn record_outcome(ctx: &TurnCtx<'_>, tool_name: &str, outcome: &ToolExecutionOutcome) {
    let threshold = ctx.pacing.tool_breaker_threshold;
    if threshold == 0 || !applies_to(tool_name) || outcome.output.contains(UNAVAILABLE_MARKER) {
        return;
    }
    let text = outcome.error_reason.as_deref().unwrap_or(&outcome.output);
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.record(
            key_for(tool_name),
            Instant::now(),
            outcome.success,
            !outcome.success && is_server_failure(text),
            text,
            threshold,
            Duration::from_secs(ctx.pacing.tool_breaker_cooldown_secs.max(1)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> (String, String) {
        ("tenant-a".into(), "mail__get_thread_memory".into())
    }
    fn fail(r: &mut BreakerRegistry, at: Instant, n: usize) {
        for i in 0..n {
            r.record(
                key(),
                at + Duration::from_millis(i as u64),
                false,
                true,
                "server down",
                5,
                Duration::from_secs(60),
            );
        }
    }

    #[test]
    fn opens_after_the_threshold_of_server_failures_and_not_before() {
        let mut r = BreakerRegistry::default();
        let t0 = Instant::now();
        fail(&mut r, t0, 4);
        assert!(r.check(&key(), t0).is_none(), "4 < 5: still closed");
        fail(&mut r, t0, 1);
        let blocked = r.check(&key(), t0).expect("5th failure opens it");
        assert_eq!(blocked.failures, 5);
        assert_eq!(blocked.last_error, "server down");
        assert!(blocked.retry_in_secs >= 59 && blocked.retry_in_secs <= 60);
    }

    #[test]
    fn half_opens_after_the_cooldown_and_doubles_when_the_probe_fails() {
        let mut r = BreakerRegistry::default();
        let t0 = Instant::now();
        fail(&mut r, t0, 5);
        let later = t0 + Duration::from_secs(61);
        assert!(
            r.check(&key(), later).is_none(),
            "cooldown over: let the probe through"
        );
        // The probe fails too: re-opens for twice as long.
        r.record(
            key(),
            later,
            false,
            true,
            "still down",
            5,
            Duration::from_secs(60),
        );
        let blocked = r.check(&key(), later).expect("re-opened");
        assert!(
            blocked.retry_in_secs > 100,
            "doubled cooldown, got {}",
            blocked.retry_in_secs
        );
        // A successful probe closes it for good.
        let much_later = later + Duration::from_secs(200);
        r.record(
            key(),
            much_later,
            true,
            false,
            "",
            5,
            Duration::from_secs(60),
        );
        assert!(r.check(&key(), much_later).is_none());
        fail(&mut r, much_later, 4);
        assert!(
            r.check(&key(), much_later).is_none(),
            "count restarted from zero"
        );
    }

    #[test]
    fn a_tool_answering_with_an_error_proves_the_server_is_up() {
        let mut r = BreakerRegistry::default();
        let t0 = Instant::now();
        fail(&mut r, t0, 4);
        // e.g. "thread not found": not a server failure, resets the count.
        r.record(
            key(),
            t0,
            false,
            false,
            "MCP `x` (server `y`) returned isError: not found",
            5,
            Duration::from_secs(60),
        );
        fail(&mut r, t0, 4);
        assert!(r.check(&key(), t0).is_none(), "4 + reset + 4 must not open");
    }

    #[test]
    fn keys_are_per_tenant_and_per_tool_and_threshold_zero_is_inert() {
        let mut r = BreakerRegistry::default();
        let t0 = Instant::now();
        fail(&mut r, t0, 5);
        assert!(
            r.check(&("tenant-b".into(), "mail__get_thread_memory".into()), t0)
                .is_none()
        );
        assert!(
            r.check(&("tenant-a".into(), "mail__search_mail".into()), t0)
                .is_none()
        );
        let mut off = BreakerRegistry::default();
        for _ in 0..20 {
            off.record(key(), t0, false, true, "down", 0, Duration::from_secs(60));
        }
        assert!(off.check(&key(), t0).is_none());
    }

    #[test]
    fn only_mcp_server_failures_are_server_failures() {
        // The wording mcp_client::dispatch_rpc produces...
        assert!(is_server_failure(
            "MCP server `tenant_aivory-mail` error during tool call `t`: HTTP 502"
        ));
        assert!(is_server_failure(
            "MCP server `x` timed out after 30s before writing tool call `t`"
        ));
        assert!(is_server_failure(
            "MCP server `x` failed during tool call `t`; outcome unknown"
        ));
        // ...and the wording of a server that ANSWERED.
        assert!(!is_server_failure(
            "MCP `t` (server `x`) returned isError: thread not found"
        ));
        assert!(!is_server_failure(
            "MCP tool `t` error -32602: invalid params"
        ));
        assert!(!is_server_failure(
            "Failed to update task: not found for this tenant"
        ));
    }

    #[test]
    fn the_cooldown_is_capped() {
        let mut r = BreakerRegistry::default();
        let t0 = Instant::now();
        fail(&mut r, t0, 40);
        let blocked = r.check(&key(), t0 + Duration::from_secs(1)).unwrap();
        assert!(blocked.retry_in_secs <= 300, "{}", blocked.retry_in_secs);
    }
}
