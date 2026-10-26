//! ADR-021: agent-to-agent communication in a Team Space room.
//!
//! Seven tools — `team_message`, `team_ask`, `team_delegate`, `team_handoff`,
//! `team_remind`, `team_wait`, `team_wait_close` — let an agent talk to its
//! teammates the way people in a room do, and park work it is blocked on. A call validates its arguments and records a [`TeamAction`] for the
//! room; it has no side effect of its own. After the turn the room carries it
//! out: it posts the message from this agent, starts the teammate's turn, sends
//! the answer back to the asker, and fires reminders when they are due. So the
//! tools are `Safe` tier and never park on an approval.
//!
//! The tools are only offered on a room turn (the gateway scopes
//! [`crate::agent::tenant::TEAM_ACTIONS`] for `X-Room-Channel: discussion`),
//! never on Telegram/Slack/console turns where nothing would deliver them.

use crate::agent::tenant::{TeamAction, current_tenant, record_team_action};
use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

/// The seven tool names, for registration and the risk-tier safe list.
pub const TEAM_TOOL_NAMES: [&str; 7] = [
    "team_message",
    "team_ask",
    "team_delegate",
    "team_handoff",
    "team_remind",
    "team_wait",
    "team_wait_close",
];

const MAX_TEXT_CHARS: usize = 2000;
const MAX_NAME_CHARS: usize = 64;
const BROADCAST: [&str; 7] = [
    "all",
    "everyone",
    "everybody",
    "here",
    "channel",
    "semua",
    "team",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamKind {
    Message,
    Ask,
    Delegate,
    Handoff,
    Remind,
    Wait,
    WaitClose,
}

impl TeamKind {
    fn tool_name(self) -> &'static str {
        match self {
            Self::Message => "team_message",
            Self::Ask => "team_ask",
            Self::Delegate => "team_delegate",
            Self::Handoff => "team_handoff",
            Self::Remind => "team_remind",
            Self::Wait => "team_wait",
            Self::WaitClose => "team_wait_close",
        }
    }

    fn action_kind(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Ask => "ask",
            Self::Delegate => "delegate",
            Self::Handoff => "handoff",
            Self::Remind => "remind",
            Self::Wait => "wait",
            Self::WaitClose => "wait_close",
        }
    }
}

/// One of the seven team communication tools.
pub struct TeamCommsTool {
    kind: TeamKind,
}

impl TeamCommsTool {
    /// All seven tools, in [`TEAM_TOOL_NAMES`] order.
    pub fn all() -> Vec<Self> {
        [
            TeamKind::Message,
            TeamKind::Ask,
            TeamKind::Delegate,
            TeamKind::Handoff,
            TeamKind::Remind,
            TeamKind::Wait,
            TeamKind::WaitClose,
        ]
        .into_iter()
        .map(|kind| Self { kind })
        .collect()
    }

    fn refuse(message: impl Into<String>) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some(message.into()),
        })
    }
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
}

fn normalized(name: &str) -> String {
    name.trim()
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .replace(['_', '-', ' '], "")
}

/// Validate a teammate reference. The room resolves names against the agents
/// actually present, so only the shape is checked here.
fn validate_teammate(to: &str, self_type: Option<&str>) -> Result<String, String> {
    let to = to.trim().trim_start_matches('@').trim();
    if to.is_empty() {
        return Err("'to' must name one teammate (their name or agent type)".into());
    }
    if to.chars().count() > MAX_NAME_CHARS
        || !to
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ' ' | '.'))
    {
        return Err(format!("'{to}' is not a teammate name"));
    }
    if BROADCAST.contains(&to.to_ascii_lowercase().as_str()) {
        return Err("message one teammate at a time; broadcasts are not allowed".into());
    }
    if self_type.is_some_and(|me| normalized(me) == normalized(to))
        || to.eq_ignore_ascii_case("self")
    {
        return Err("you cannot message yourself; use team_remind with who='self' instead".into());
    }
    Ok(to.to_string())
}

fn validate_text(text: &str, field: &str) -> Result<String, String> {
    if text.is_empty() {
        return Err(format!("'{field}' must not be empty"));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "'{field}' is longer than {MAX_TEXT_CHARS} characters"
        ));
    }
    Ok(text.to_string())
}

/// `due` for a delegation: an RFC 3339 timestamp or a `YYYY-MM-DD` date.
fn validate_due(due: &str) -> Result<Option<String>, String> {
    if due.is_empty() {
        return Ok(None);
    }
    let ok = chrono::DateTime::parse_from_rfc3339(due).is_ok()
        || chrono::NaiveDate::parse_from_str(due, "%Y-%m-%d").is_ok();
    if ok {
        Ok(Some(due.to_string()))
    } else {
        Err("'due' must be a date (YYYY-MM-DD) or an ISO 8601 timestamp".into())
    }
}

/// Minutes from now for a reminder `when`: `in 30m`, `in 2h`, `in 3d`, or an
/// RFC 3339 timestamp. `None` when unparseable.
pub(crate) fn reminder_delay_minutes(
    when: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<i64> {
    let when = when.trim();
    if let Some(rest) = when.strip_prefix("in ").or_else(|| when.strip_prefix("in")) {
        let rest = rest.trim();
        let split = rest.find(|c: char| !c.is_ascii_digit())?;
        let (number, unit) = rest.split_at(split);
        let n: i64 = number.parse().ok()?;
        let minutes = match unit.trim() {
            "m" | "min" | "mins" | "minute" | "minutes" | "menit" => n,
            "h" | "hr" | "hrs" | "hour" | "hours" | "jam" => n * 60,
            "d" | "day" | "days" | "hari" => n * 60 * 24,
            _ => return None,
        };
        return Some(minutes);
    }
    let at = chrono::DateTime::parse_from_rfc3339(when).ok()?;
    Some((at.with_timezone(&chrono::Utc) - now).num_minutes())
}

/// `if_missed` for a reminder: `run_once` | `skip`, or the room's default.
fn validate_missed(missed: &str) -> Result<Option<String>, String> {
    match missed.to_ascii_lowercase().replace('-', "_").as_str() {
        "" => Ok(None),
        p @ ("run_once" | "skip") => Ok(Some(p.to_string())),
        _ => Err("'if_missed' must be 'run_once' or 'skip'".into()),
    }
}

/// A wait id as the room hands it out (`[Wait w_…]` in a woken turn).
fn validate_wait_id(id: &str) -> Result<Option<String>, String> {
    if id.is_empty() {
        return Ok(None);
    }
    if id.len() > MAX_NAME_CHARS
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(format!("'{id}' is not a wait id"));
    }
    Ok(Some(id.to_string()))
}

fn validate_when(when: &str) -> Result<String, String> {
    if when.is_empty() {
        return Err("'when' is required: an ISO 8601 time or 'in 30m' / 'in 2h' / 'in 3d'".into());
    }
    match reminder_delay_minutes(when, chrono::Utc::now()) {
        None => Err("'when' must be an ISO 8601 time or 'in 30m' / 'in 2h' / 'in 3d'".into()),
        Some(m) if m < 1 => Err("a reminder must be at least one minute in the future".into()),
        Some(m) if m > 30 * 24 * 60 => Err("a reminder can be at most 30 days out".into()),
        Some(_) => Ok(when.to_string()),
    }
}

#[async_trait]
impl Tool for TeamCommsTool {
    fn name(&self) -> &str {
        self.kind.tool_name()
    }

    fn description(&self) -> &str {
        match self.kind {
            TeamKind::Message => {
                "Post an FYI to one teammate in this room (they see it, no reply expected). \
                 Use it to share a finding or status. It is posted as your message after this turn."
            }
            TeamKind::Ask => {
                "Ask one teammate in this room a question you need answered to continue. \
                 It is posted after this turn and starts their turn (or queues behind their current \
                 one); when they answer you get a new turn with the answer. Tell the user you are \
                 checking with them, then stop. Do not ask the same thing twice, and do not reply \
                 to an answer just to acknowledge it."
            }
            TeamKind::Delegate => {
                "Assign a piece of work to one teammate in this room, optionally with a due date. \
                 It becomes a tracked task on the board and starts their turn; you get their report \
                 (Status: done | partial | blocked, Result, Evidence, Unblock) in a new turn. Describe \
                 the task fully: they cannot see your private context. Never send a task back to the \
                 teammate who gave it to you."
            }
            TeamKind::Handoff => {
                "Hand this conversation over to one teammate in this room who is better placed to \
                 handle it. It starts their turn and you are done with this thread."
            }
            TeamKind::Remind => {
                "Set a reminder for yourself (who='self') or a teammate in this room. When it is due \
                 the room posts it and starts their turn, e.g. to follow up on an unpaid invoice. \
                 It fires even when nobody has the room open."
            }
            TeamKind::Wait => {
                "Park the request you are working on because you are blocked on something outside \
                 this turn (a teammate's delegated task, a build, a reply from a customer). The room \
                 keeps the request and your note, and gives you a new turn that repeats both when the \
                 wait wakes: at 'until', when a task you delegated finishes, or when a webhook wakes it. \
                 Whenever the request names a time or a delay ('in 2 minutes', 'tomorrow at 9', \
                 'after an hour'), pass it as 'until': without 'until' the wait never wakes on a timer."
            }
            TeamKind::WaitClose => {
                "Close a wait you opened with team_wait, because the thing you were waiting for \
                 happened or no longer matters. Without wait_id it closes all your open waits here."
            }
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let teammate = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_CHARS,
            "description": "One teammate in this room: their name (e.g. 'Lex') or agent type (e.g. 'leads_qualifier')."
        });
        match self.kind {
            TeamKind::Message | TeamKind::Handoff => json!({
                "type": "object",
                "properties": {
                    "to": teammate,
                    "text": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS,
                              "description": "What to tell them." }
                },
                "required": ["to", "text"],
                "additionalProperties": false
            }),
            TeamKind::Ask => json!({
                "type": "object",
                "properties": {
                    "to": teammate,
                    "question": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS,
                                  "description": "The question, with the context they need to answer it." }
                },
                "required": ["to", "question"],
                "additionalProperties": false
            }),
            TeamKind::Delegate => json!({
                "type": "object",
                "properties": {
                    "to": teammate,
                    "task": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS,
                              "description": "The work, described fully (what, for whom, what done looks like)." },
                    "due": { "type": "string",
                             "description": "Optional due date: YYYY-MM-DD or an ISO 8601 timestamp." }
                },
                "required": ["to", "task"],
                "additionalProperties": false
            }),
            TeamKind::Remind => json!({
                "type": "object",
                "properties": {
                    "who": { "type": "string", "maxLength": MAX_NAME_CHARS,
                             "description": "'self' (default) or one teammate in this room." },
                    "when": { "type": "string",
                              "description": "ISO 8601 time, or relative: 'in 30m', 'in 2h', 'in 3d' (1 minute to 30 days)." },
                    "text": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS,
                              "description": "What to be reminded of." },
                    "if_missed": { "type": "string", "enum": ["run_once", "skip"],
                                   "description": "If it could not fire on time: fire once late (default) or skip it." }
                },
                "required": ["when", "text"],
                "additionalProperties": false
            }),
            TeamKind::Wait => json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS,
                              "description": "What you are waiting for and what to do once it arrives." },
                    "until": { "type": "string",
                               "description": "When to wake: ISO 8601, or 'in 30m' / 'in 2h' / 'in 3d' (1 minute to 30 days). Set it whenever the request gives a time or delay; leave it out only when the wait should wake solely on a teammate's result or a webhook." }
                },
                "required": ["note"],
                "additionalProperties": false
            }),
            TeamKind::WaitClose => json!({
                "type": "object",
                "properties": {
                    "wait_id": { "type": "string", "maxLength": MAX_NAME_CHARS,
                                 "description": "The wait to close (from the woken turn). Omit to close all of yours here." },
                    "note": { "type": "string", "maxLength": MAX_TEXT_CHARS,
                              "description": "Optional: how it ended." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let self_type = current_tenant().map(|t| t.agent_type.clone());
        let action = match self.kind {
            TeamKind::Remind => {
                let who = str_arg(&args, "who");
                let to = if who.is_empty() || who.eq_ignore_ascii_case("self") {
                    "self".to_string()
                } else {
                    match validate_teammate(who, self_type.as_deref()) {
                        Ok(to) => to,
                        Err(e) => return Self::refuse(e),
                    }
                };
                let when = match validate_when(str_arg(&args, "when")) {
                    Ok(w) => w,
                    Err(e) => return Self::refuse(e),
                };
                let text = match validate_text(str_arg(&args, "text"), "text") {
                    Ok(t) => t,
                    Err(e) => return Self::refuse(e),
                };
                let missed_policy = match validate_missed(str_arg(&args, "if_missed")) {
                    Ok(p) => p,
                    Err(e) => return Self::refuse(e),
                };
                TeamAction {
                    kind: self.kind.action_kind().into(),
                    to,
                    text,
                    due: None,
                    when: Some(when),
                    missed_policy,
                    wait_id: None,
                }
            }
            TeamKind::Wait => {
                let text = match validate_text(str_arg(&args, "note"), "note") {
                    Ok(t) => t,
                    Err(e) => return Self::refuse(e),
                };
                let until = str_arg(&args, "until");
                let when = if until.is_empty() {
                    None
                } else {
                    match validate_when(until) {
                        Ok(w) => Some(w),
                        Err(e) => return Self::refuse(e.replace("'when'", "'until'")),
                    }
                };
                TeamAction {
                    kind: self.kind.action_kind().into(),
                    to: "self".into(),
                    text,
                    due: None,
                    when,
                    missed_policy: None,
                    wait_id: None,
                }
            }
            TeamKind::WaitClose => {
                let wait_id = match validate_wait_id(str_arg(&args, "wait_id")) {
                    Ok(id) => id,
                    Err(e) => return Self::refuse(e),
                };
                let note = str_arg(&args, "note");
                if note.chars().count() > MAX_TEXT_CHARS {
                    return Self::refuse(format!(
                        "'note' is longer than {MAX_TEXT_CHARS} characters"
                    ));
                }
                TeamAction {
                    kind: self.kind.action_kind().into(),
                    to: "self".into(),
                    text: note.to_string(),
                    due: None,
                    when: None,
                    missed_policy: None,
                    wait_id,
                }
            }
            kind => {
                let to = match validate_teammate(str_arg(&args, "to"), self_type.as_deref()) {
                    Ok(to) => to,
                    Err(e) => return Self::refuse(e),
                };
                let field = match kind {
                    TeamKind::Ask => "question",
                    TeamKind::Delegate => "task",
                    _ => "text",
                };
                let text = match validate_text(str_arg(&args, field), field) {
                    Ok(t) => t,
                    Err(e) => return Self::refuse(e),
                };
                let due = if kind == TeamKind::Delegate {
                    match validate_due(str_arg(&args, "due")) {
                        Ok(d) => d,
                        Err(e) => return Self::refuse(e),
                    }
                } else {
                    None
                };
                TeamAction {
                    kind: kind.action_kind().into(),
                    to,
                    text,
                    due,
                    when: None,
                    missed_policy: None,
                    wait_id: None,
                }
            }
        };

        let to = action.to.clone();
        if let Err(e) = record_team_action(action) {
            return Self::refuse(e);
        }
        let note = match self.kind {
            TeamKind::Message => {
                format!("Queued: your note to {to} is posted in the room after this turn.")
            }
            TeamKind::Ask => format!(
                "Queued: your question to {to} is posted after this turn and starts their turn. \
                 You will get a new turn with their answer, so tell the user you are checking with \
                 {to} and end this turn."
            ),
            TeamKind::Delegate => format!(
                "Queued: the task for {to} goes on the board after this turn and starts their turn. \
                 You will be told when it is done."
            ),
            TeamKind::Handoff => format!(
                "Queued: {to} takes over this thread after this turn. Tell the user who is taking over."
            ),
            TeamKind::Remind => {
                format!("Queued: the reminder for {to} is scheduled after this turn.")
            }
            TeamKind::Wait => "Queued: the room keeps this request and your note after this turn \
                 and gives you a new turn when the wait wakes. Tell the user what you are waiting \
                 for and end this turn."
                .to_string(),
            TeamKind::WaitClose => "Queued: the wait is closed after this turn.".to_string(),
        };
        Ok(ToolResult {
            success: true,
            output: note.into(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tenant::{TEAM_ACTIONS, take_team_actions};
    use std::sync::Arc;

    fn tool(name: &str) -> TeamCommsTool {
        TeamCommsTool::all()
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap()
    }

    async fn in_room<F: std::future::Future>(f: F) -> (F::Output, Vec<TeamAction>) {
        TEAM_ACTIONS
            .scope(Some(Arc::new(parking_lot::Mutex::new(Vec::new()))), async {
                let out = f.await;
                (out, take_team_actions())
            })
            .await
    }

    #[tokio::test]
    async fn ask_records_one_action_for_the_room() {
        let (result, actions) = in_room(tool("team_ask").execute(json!({
            "to": "@Lex", "question": "Is Toko Melati a qualified lead?"
        })))
        .await;
        let result = result.unwrap();
        assert!(result.success, "{result:?}");
        assert_eq!(
            actions,
            vec![TeamAction {
                kind: "ask".into(),
                to: "Lex".into(),
                text: "Is Toko Melati a qualified lead?".into(),
                due: None,
                when: None,
                missed_policy: None,
                wait_id: None,
            }]
        );
    }

    #[tokio::test]
    async fn outside_a_room_turn_the_tool_refuses() {
        let result = tool("team_message")
            .execute(json!({"to": "Lex", "text": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Team Space room"));
    }

    #[tokio::test]
    async fn broadcast_and_empty_targets_are_refused() {
        for to in ["all", "@here", "", "semua"] {
            let (result, actions) =
                in_room(tool("team_handoff").execute(json!({"to": to, "text": "x"}))).await;
            assert!(!result.unwrap().success, "{to:?} must be refused");
            assert!(actions.is_empty());
        }
    }

    #[tokio::test]
    async fn at_most_three_actions_per_turn() {
        let (results, actions) = in_room(async {
            let mut out = Vec::new();
            for i in 0..4 {
                out.push(
                    tool("team_message")
                        .execute(json!({"to": "Lex", "text": format!("note {i}")}))
                        .await
                        .unwrap()
                        .success,
                );
            }
            out
        })
        .await;
        assert_eq!(results, vec![true, true, true, false]);
        assert_eq!(actions.len(), 3);
    }

    #[tokio::test]
    async fn delegate_validates_due_and_remind_validates_when() {
        let (bad_due, _) = in_room(tool("team_delegate").execute(json!({
            "to": "Lex", "task": "draft the quote", "due": "next friday"
        })))
        .await;
        assert!(!bad_due.unwrap().success);
        let (ok, actions) = in_room(tool("team_delegate").execute(json!({
            "to": "Lex", "task": "draft the quote", "due": "2026-11-01"
        })))
        .await;
        assert!(ok.unwrap().success);
        assert_eq!(actions[0].due.as_deref(), Some("2026-11-01"));

        let (too_far, _) = in_room(tool("team_remind").execute(json!({
            "when": "in 40d", "text": "follow up"
        })))
        .await;
        assert!(!too_far.unwrap().success);
        let (remind, actions) = in_room(tool("team_remind").execute(json!({
            "when": "in 2h", "text": "check the invoice"
        })))
        .await;
        assert!(remind.unwrap().success);
        assert_eq!(actions[0].to, "self");
        assert_eq!(actions[0].when.as_deref(), Some("in 2h"));
    }

    #[tokio::test]
    async fn remind_takes_an_optional_missed_policy() {
        let (ok, actions) = in_room(tool("team_remind").execute(json!({
            "when": "in 1d", "text": "standup digest", "if_missed": "skip"
        })))
        .await;
        assert!(ok.unwrap().success);
        assert_eq!(actions[0].missed_policy.as_deref(), Some("skip"));
        let (bad, actions) = in_room(tool("team_remind").execute(json!({
            "when": "in 1d", "text": "x", "if_missed": "later"
        })))
        .await;
        assert!(!bad.unwrap().success);
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn wait_records_the_note_and_an_optional_until() {
        let (ok, actions) = in_room(tool("team_wait").execute(json!({
            "note": "waiting for Lex's quote, then send it to Toko Melati", "until": "in 2h"
        })))
        .await;
        assert!(ok.unwrap().success);
        assert_eq!(actions[0].kind, "wait");
        assert_eq!(actions[0].to, "self");
        assert_eq!(actions[0].when.as_deref(), Some("in 2h"));

        let (no_until, actions) =
            in_room(tool("team_wait").execute(json!({"note": "the deploy"}))).await;
        assert!(no_until.unwrap().success);
        assert_eq!(actions[0].when, None);

        let (bad, _) =
            in_room(tool("team_wait").execute(json!({"note": "x", "until": "soon"}))).await;
        let err = bad.unwrap().error.unwrap();
        assert!(err.contains("'until'"), "{err}");
        let (empty, _) = in_room(tool("team_wait").execute(json!({"note": ""}))).await;
        assert!(!empty.unwrap().success);
    }

    #[test]
    fn wait_tells_the_model_to_pass_a_requested_time_as_until() {
        let wait = tool("team_wait");
        assert!(wait.description().contains("pass it as 'until'"));
        let until = &wait.parameters_schema()["properties"]["until"]["description"];
        assert!(
            until
                .as_str()
                .unwrap()
                .contains("Set it whenever the request gives a time")
        );
    }

    #[tokio::test]
    async fn wait_close_takes_an_optional_wait_id() {
        let (ok, actions) = in_room(
            tool("team_wait_close").execute(json!({"wait_id": "w_abc123", "note": "quote sent"})),
        )
        .await;
        assert!(ok.unwrap().success);
        assert_eq!(actions[0].kind, "wait_close");
        assert_eq!(actions[0].wait_id.as_deref(), Some("w_abc123"));
        assert_eq!(actions[0].text, "quote sent");

        let (all, actions) = in_room(tool("team_wait_close").execute(json!({}))).await;
        assert!(all.unwrap().success);
        assert_eq!(actions[0].wait_id, None);

        let (bad, _) =
            in_room(tool("team_wait_close").execute(json!({"wait_id": "drop table;"}))).await;
        assert!(!bad.unwrap().success);
    }

    #[test]
    fn wait_and_remind_fields_are_omitted_from_the_wire_when_unset() {
        let action = TeamAction {
            kind: "ask".into(),
            to: "Lex".into(),
            text: "q".into(),
            due: None,
            when: None,
            missed_policy: None,
            wait_id: None,
        };
        assert_eq!(
            serde_json::to_value(&action).unwrap(),
            json!({"kind": "ask", "to": "Lex", "text": "q"})
        );
    }

    #[test]
    fn relative_reminder_delays_parse() {
        let now = chrono::Utc::now();
        assert_eq!(reminder_delay_minutes("in 30m", now), Some(30));
        assert_eq!(reminder_delay_minutes("in 2h", now), Some(120));
        assert_eq!(reminder_delay_minutes("in 3d", now), Some(4320));
        assert_eq!(reminder_delay_minutes("in 2 jam", now), Some(120));
        assert_eq!(reminder_delay_minutes("tomorrow", now), None);
    }
}
