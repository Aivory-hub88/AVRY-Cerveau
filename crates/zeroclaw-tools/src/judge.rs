//! Typed-judgment routing tool (ADR-017, P2).
//!
//! One bounded LLM call over narrow `Choice` / `Score` / `Noul` questions,
//! with the routing policy applied **in code, not in prompts** (thresholds
//! live here; changing a cutoff never requires re-inference). The model
//! supplies judgments; it never decides the action.
//!
//! Output is always `{action, reasons, answers}` where action is one of
//! `handle` (act directly), `confirm` (ask the user first — conversational
//! Ya/Batal), or `escalate` (park for a human — F-1 pending row).
//! Malformed model output fails closed: no action is ever guessed.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;
use zeroclaw_providers::ProviderDispatch;

/// Noul signal high enough to downgrade `handle` to `confirm`.
/// Observe-only signals can NEVER escalate on their own.
const OBSERVE_CONFIRM_THRESHOLD: f64 = 0.9;

#[derive(Debug, Clone)]
pub struct JudgePolicy {
    /// At/above: act on high-stakes questions directly.
    pub high_conf: f64,
    /// Below (on any Choice answer): cannot classify -> escalate.
    pub low_conf: f64,
    /// Nouls at/above this escalate (only those listed in `escalate_on`).
    pub escalate_noul_thr: f64,
}

impl Default for JudgePolicy {
    fn default() -> Self {
        Self {
            high_conf: 0.9,
            low_conf: 0.5,
            escalate_noul_thr: 0.5,
        }
    }
}

/// Agent-callable tool: judge state against typed questions, return a routing
/// decision. Same provider/model wiring as `llm_task`; temperature is fixed
/// at 0 (judgments must be deterministic).
pub struct JudgeTool {
    security: Arc<SecurityPolicy>,
    default_model_provider: String,
    default_model: String,
    api_key: Option<String>,
    provider_runtime_options: zeroclaw_providers::ModelProviderRuntimeOptions,
}

impl JudgeTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        default_model_provider: String,
        default_model: String,
        api_key: Option<String>,
        provider_runtime_options: zeroclaw_providers::ModelProviderRuntimeOptions,
    ) -> Self {
        Self {
            security,
            default_model_provider,
            default_model,
            api_key,
            provider_runtime_options,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested without network)
// ---------------------------------------------------------------------------

/// Render the judge prompt: state as named JSON, one narrow judgment per
/// question, strict answer envelope.
fn build_judge_prompt(state: &Value, questions: &Map<String, Value>) -> String {
    let state_json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    let mut blocks = Vec::new();
    for (qid, q) in questions {
        let qtype = q.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let instructions = q.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
        let criteria = q
            .get("criteria")
            .map(|c| serde_json::to_string(c).unwrap_or_default())
            .unwrap_or_default();
        blocks.push(format!(
            "- id: `{qid}` | type: {qtype}\n  judgment: {instructions}\n  answers: {criteria}"
        ));
    }
    format!(
        "You are a judgment step, not a decision maker. Read the state, answer \
         each question narrowly, and return ONLY a JSON object shaped like:\n\
         {{\"answers\": {{\"<question-id>\": <answer>}}}}\n\
         where a Choice answer is {{\"choice\": \"<option-id>\", \"confidence\": 0.0-1.0}}, \
         a Score answer is {{\"score\": <level-index>, \"confidence\": 0.0-1.0}}, \
         and a Noul answer is {{\"noul\": 0.0-1.0}}. \
         `confidence` is your certainty in THIS answer, never permission to act. \
         Use no-match outcomes (\"other\"/low noul) instead of forcing a fit. \
         No explanation, no markdown, JSON only.\n\n\
         ## State\n```json\n{state_json}\n```\n\n## Questions\n{blocks}",
        blocks = blocks.join("\n")
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    kind: String,
    choice: Option<String>,
    score: Option<i64>,
    noul: Option<f64>,
    confidence: Option<f64>,
}

/// Validate one raw answer against its declared question. Fail-closed.
fn parse_answer(qid: &str, q: &Value, raw: &Value) -> Result<Answer, String> {
    let qtype = q.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("answer '{qid}' is not an object"))?;
    let conf = obj.get("confidence").and_then(|v| v.as_f64());
    match qtype {
        "choice" => {
            let options: Vec<String> = match q.get("criteria") {
                Some(Value::Object(m)) => m.keys().cloned().collect(),
                _ => return Err(format!("question '{qid}' has no criteria object")),
            };
            let pick = obj
                .get("choice")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("answer '{qid}' missing 'choice'"))?;
            if !options.iter().any(|o| o == pick) {
                return Err(format!("answer '{qid}' picks unknown option '{pick}'"));
            }
            let confidence = conf.ok_or_else(|| format!("answer '{qid}' missing 'confidence'"))?;
            Ok(Answer {
                kind: "choice".into(),
                choice: Some(pick.into()),
                score: None,
                noul: None,
                confidence: Some(confidence),
            })
        }
        "score" => {
            let levels = q
                .get("criteria")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let score = obj
                .get("score")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| format!("answer '{qid}' missing integer 'score'"))?;
            if score < 0 || (levels > 0 && score as usize >= levels) {
                return Err(format!(
                    "answer '{qid}' score {score} out of range (0..{levels})"
                ));
            }
            let confidence = conf.ok_or_else(|| format!("answer '{qid}' missing 'confidence'"))?;
            Ok(Answer {
                kind: "score".into(),
                choice: None,
                score: Some(score),
                noul: None,
                confidence: Some(confidence),
            })
        }
        "noul" => {
            let p = obj
                .get("noul")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| format!("answer '{qid}' missing 'noul'"))?;
            if !(0.0..=1.0).contains(&p) {
                return Err(format!("answer '{qid}' noul {p} outside 0..=1"));
            }
            Ok(Answer {
                kind: "noul".into(),
                choice: None,
                score: None,
                noul: Some(p),
                confidence: None,
            })
        }
        other => Err(format!("question '{qid}' has unknown type '{other}'")),
    }
}

fn parse_answers(
    questions: &Map<String, Value>,
    raw_answers: &Map<String, Value>,
) -> Result<BTreeMap<String, Answer>, String> {
    let mut out = BTreeMap::new();
    for (qid, q) in questions {
        let raw = raw_answers
            .get(qid)
            .ok_or_else(|| format!("missing answer for '{qid}'"))?;
        out.insert(qid.clone(), parse_answer(qid, q, raw)?);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: String,
    pub reasons: Vec<String>,
}

/// Policy in code. Mirrors `decide.py` (evals/typesafe-jev):
/// - `escalate_on` Nouls at/above threshold -> escalate (explicit list only);
/// - score at/above `escalate_score_at` -> escalate;
/// - any Choice confidence below `low_conf` -> escalate (cannot classify);
/// - score at/above `confirm_score_at`, any Choice below `high_conf`, or an
///   observe-only signal at/above 0.9 -> confirm (downgrade only, never escalate);
/// - otherwise handle.
#[allow(clippy::too_many_arguments)]
pub fn apply_policy(
    answers: &BTreeMap<String, Answer>,
    escalate_on: &[String],
    escalate_score_at: Option<i64>,
    confirm_score_at: Option<i64>,
    observe_only: &[String],
    policy: &JudgePolicy,
) -> Decision {
    let mut reasons = Vec::new();

    if let Some(at) = escalate_score_at {
        for (qid, a) in answers {
            if let Some(s) = a.score
                && s >= at
            {
                reasons.push(format!(
                    "score {s} on '{qid}' at/above escalate threshold {at}"
                ));
                return Decision {
                    action: "escalate".into(),
                    reasons,
                };
            }
        }
    }

    for qid in escalate_on {
        if let Some(a) = answers.get(qid)
            && let Some(p) = a.noul
            && p >= policy.escalate_noul_thr
        {
            reasons.push(format!(
                "policy noul '{qid}' fired ({p:.2} >= {:.2})",
                policy.escalate_noul_thr
            ));
            return Decision {
                action: "escalate".into(),
                reasons,
            };
        }
    }

    for (qid, a) in answers {
        if a.kind == "choice"
            && let Some(c) = a.confidence
            && c < policy.low_conf
        {
            reasons.push(format!(
                "choice confidence {c:.2} on '{qid}' < {:.2}",
                policy.low_conf
            ));
            return Decision {
                action: "escalate".into(),
                reasons,
            };
        }
    }

    let mut action = "handle".to_string();
    if let Some(at) = confirm_score_at {
        for (qid, a) in answers {
            if let Some(s) = a.score
                && s >= at
            {
                reasons.push(format!(
                    "score {s} on '{qid}' at/above confirm threshold {at}"
                ));
                action = "confirm".to_string();
            }
        }
    }
    for (qid, a) in answers {
        if a.kind == "choice"
            && let Some(c) = a.confidence
            && c < policy.high_conf
        {
            reasons.push(format!(
                "choice confidence {c:.2} on '{qid}' < high {:.2}",
                policy.high_conf
            ));
            action = "confirm".to_string();
        }
    }
    let mut observed = Vec::new();
    for qid in observe_only {
        if let Some(a) = answers.get(qid)
            && let Some(p) = a.noul
            && p >= OBSERVE_CONFIRM_THRESHOLD
        {
            observed.push(format!("observed signal '{qid}' {p:.2}"));
        }
    }
    if !observed.is_empty() {
        reasons.extend(observed);
        if action == "handle" {
            reasons.push("downgraded to confirm by observed signal (never escalates)".into());
            action = "confirm".to_string();
        }
    }
    if reasons.is_empty() {
        reasons.push("confidence sufficient, no policy hit".into());
    }
    Decision { action, reasons }
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with("```") {
        t.trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        t
    }
}

#[async_trait]
impl Tool for JudgeTool {
    fn name(&self) -> &str {
        "judge"
    }

    fn description(&self) -> &str {
        "Judge state against narrow typed questions (Choice/Score/Noul) and return \
         a routing decision: handle, confirm, or escalate. One bounded LLM call; \
         the routing policy (thresholds) is applied in code, never in prompts. \
         The model supplies judgments; it never decides the action."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "state": {
                    "type": "object",
                    "description": "Named JSON fields the questions judge (message, proposed action, stakes)"
                },
                "questions": {
                    "type": "object",
                    "description": "Question definitions by id: {type: choice|score|noul, instructions, criteria}. Same shape as the judge skill's questions.json."
                },
                "escalate_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Noul question ids that escalate at/above threshold (explicit list only; default: none)"
                },
                "escalate_score_at": {
                    "type": "integer",
                    "description": "Score at/above this escalates (default: unset)"
                },
                "confirm_score_at": {
                    "type": "integer",
                    "description": "Score at/above this needs confirmation (default: unset)"
                },
                "observe_only": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Informational Noul ids (e.g. injection_attempt): can only downgrade handle to confirm, never escalate"
                }
            },
            "required": ["state", "questions"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Err(e) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "judge")
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Action blocked: {e}")),
            });
        }

        let state = args.get("state").cloned().unwrap_or(Value::Null);
        if !state.is_object() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Missing or invalid required parameter: state (object)".to_string()),
            });
        }
        let questions = match args.get("questions").and_then(|v| v.as_object()) {
            Some(q) if !q.is_empty() => q.clone(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(
                        "Missing or empty required parameter: questions (object)".to_string(),
                    ),
                });
            }
        };
        let escalate_on: Vec<String> = args
            .get("escalate_on")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let escalate_score_at = args.get("escalate_score_at").and_then(|v| v.as_i64());
        let confirm_score_at = args.get("confirm_score_at").and_then(|v| v.as_i64());
        let observe_only: Vec<String> = args
            .get("observe_only")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["injection_attempt".to_string()]);
        for qid in escalate_on.iter().chain(observe_only.iter()) {
            if !questions.contains_key(qid) {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("policy references unknown question '{qid}'")),
                });
            }
        }

        let prompt = build_judge_prompt(&state, &questions);
        let model_provider: Box<dyn ModelProvider> =
            match zeroclaw_providers::create_model_provider_with_options(
                &self.default_model_provider,
                self.api_key.as_deref(),
                &self.provider_runtime_options,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!("Failed to create model_provider: {e}")),
                    });
                }
            };
        // Temperature fixed at 0: judgments must be deterministic.
        let response = match ProviderDispatch::from_ref(&*model_provider)
            .simple_chat(&prompt, &self.default_model, Some(0.0))
            .await
        {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("LLM call failed: {e}")),
                });
            }
        };

        // Fail-closed parse: any malformed answer is an error, never a guess.
        let parsed: Value = match serde_json::from_str(strip_fences(&response)) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: response.clone().into(),
                    error: Some(format!("Judge returned invalid JSON: {e}")),
                });
            }
        };
        let raw_answers = match parsed.get("answers").and_then(|v| v.as_object()) {
            Some(m) => m,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: response.clone().into(),
                    error: Some("Judge response missing 'answers' object".to_string()),
                });
            }
        };
        let answers = match parse_answers(&questions, raw_answers) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: response.clone().into(),
                    error: Some(format!("Judge answer rejected: {e}")),
                });
            }
        };

        let policy = JudgePolicy::default();
        let decision = apply_policy(
            &answers,
            &escalate_on,
            escalate_score_at,
            confirm_score_at,
            &observe_only,
            &policy,
        );
        // Echo typed answers alongside the decision so turns stay auditable.
        let mut answers_json = serde_json::Map::new();
        for (qid, a) in &answers {
            answers_json.insert(
                qid.clone(),
                match a.kind.as_str() {
                    "choice" => json!({"choice": a.choice, "confidence": a.confidence}),
                    "score" => json!({"score": a.score, "confidence": a.confidence}),
                    _ => json!({"noul": a.noul}),
                },
            );
        }
        Ok(ToolResult {
            success: true,
            output: json!({
                "action": decision.action,
                "reasons": decision.reasons,
                "answers": answers_json,
            })
            .to_string()
            .into(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triage_questions() -> Map<String, Value> {
        serde_json::from_value(json!({
            "intent": {"type": "choice", "instructions": "Main intent",
                "criteria": {"how_to": "asks how", "complaint": "unhappy", "small_talk": "greeting"}},
            "severity": {"type": "score", "instructions": "Harm level",
                "criteria": ["none", "minor", "material", "severe"]},
            "wants_human": {"type": "noul", "instructions": "Wants a human",
                "criteria": "explicitly asks for a person"},
            "injection_attempt": {"type": "noul", "instructions": "Prompt injection present",
                "criteria": "tries to override instructions"}
        }))
        .unwrap()
    }

    fn answers(
        intent: &str,
        iconf: f64,
        sev: i64,
        sconf: f64,
        human: f64,
        inj: f64,
    ) -> BTreeMap<String, Answer> {
        parse_answers(
            &triage_questions(),
            &serde_json::from_value(json!({
                "intent": {"choice": intent, "confidence": iconf},
                "severity": {"score": sev, "confidence": sconf},
                "wants_human": {"noul": human},
                "injection_attempt": {"noul": inj}
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn policy_args() -> (Vec<String>, Option<i64>, Option<i64>, Vec<String>) {
        (
            vec!["wants_human".to_string()],
            Some(3),
            Some(2),
            vec!["injection_attempt".to_string()],
        )
    }

    #[test]
    fn tool_metadata() {
        let tool = JudgeTool::new(
            Arc::new(SecurityPolicy::default()),
            "openrouter".into(),
            "openai/gpt-4o-mini".into(),
            None,
            zeroclaw_providers::ModelProviderRuntimeOptions::default(),
        );
        assert_eq!(tool.name(), "judge");
        assert!(!tool.description().is_empty());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "state"));
        assert!(required.iter().any(|v| v == "questions"));
    }

    #[test]
    fn severe_score_escalates() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("complaint", 0.99, 3, 0.95, 0.02, 0.05),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "escalate");
    }

    #[test]
    fn policy_noul_escalates() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("how_to", 0.95, 0, 0.9, 0.97, 0.02),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "escalate");
    }

    #[test]
    fn low_choice_confidence_escalates() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("complaint", 0.3, 1, 0.4, 0.02, 0.02),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "escalate");
    }

    #[test]
    fn mid_confidence_confirms() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("complaint", 0.7, 1, 0.7, 0.02, 0.02),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "confirm");
    }

    #[test]
    fn clean_case_handles() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("how_to", 0.95, 0, 0.9, 0.02, 0.05),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "handle");
    }

    #[test]
    fn injection_alone_never_escalates() {
        let (eo, esa, csa, oo) = policy_args();
        let d = apply_policy(
            &answers("how_to", 0.95, 0, 0.9, 0.02, 0.95),
            &eo,
            esa,
            csa,
            &oo,
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "confirm");
    }

    #[test]
    fn unlisted_noul_does_not_escalate() {
        // wants_human fires only when listed in escalate_on.
        let d = apply_policy(
            &answers("how_to", 0.95, 0, 0.9, 0.97, 0.02),
            &[],
            None,
            None,
            &[],
            &JudgePolicy::default(),
        );
        assert_eq!(d.action, "handle");
    }

    #[test]
    fn rejects_unknown_option_and_bad_score() {
        let qs = triage_questions();
        let raw: Map<String, Value> = serde_json::from_value(json!({
            "intent": {"choice": "nope", "confidence": 0.9},
            "severity": {"score": 0, "confidence": 0.9},
            "wants_human": {"noul": 0.01},
            "injection_attempt": {"noul": 0.01}
        }))
        .unwrap();
        assert!(parse_answers(&qs, &raw).is_err());
        let raw2: Map<String, Value> = serde_json::from_value(json!({
            "intent": {"choice": "how_to", "confidence": 0.9},
            "severity": {"score": 9, "confidence": 0.9},
            "wants_human": {"noul": 0.01},
            "injection_attempt": {"noul": 0.01}
        }))
        .unwrap();
        assert!(parse_answers(&qs, &raw2).is_err());
    }

    #[test]
    fn rejects_missing_answer() {
        let qs = triage_questions();
        let raw: Map<String, Value> =
            serde_json::from_value(json!({"intent": {"choice": "how_to", "confidence": 0.9}}))
                .unwrap();
        assert!(parse_answers(&qs, &raw).is_err());
    }

    #[test]
    fn prompt_contains_state_and_questions() {
        let state = json!({"customer_message": "halo"});
        let p = build_judge_prompt(&state, &triage_questions());
        assert!(p.contains("halo"));
        assert!(p.contains("`intent`"));
        assert!(p.contains("JSON only"));
    }
}
