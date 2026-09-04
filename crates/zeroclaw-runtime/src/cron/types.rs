use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeroclaw_config::schema::CronShellOutputFormat;

pub fn deserialize_maybe_stringified<T: serde::de::DeserializeOwned>(
    v: &serde_json::Value,
) -> Result<T, serde_json::Error> {
    // Fast path: value is already the right shape (object, array, etc.)
    match serde_json::from_value::<T>(v.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(first_err) => {
            // If it's a string, try parsing the string as JSON first.
            if let Some(s) = v.as_str() {
                let s = s.trim();
                if (s.starts_with('{') || s.starts_with('['))
                    && let Ok(inner) = serde_json::from_str::<serde_json::Value>(s)
                {
                    return serde_json::from_value::<T>(inner);
                }
            }
            Err(first_err)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum JobType {
    #[default]
    Shell,
    Agent,
}

impl From<JobType> for &'static str {
    fn from(value: JobType) -> Self {
        match value {
            JobType::Shell => "shell",
            JobType::Agent => "agent",
        }
    }
}

impl TryFrom<&str> for JobType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_lowercase().as_str() {
            "shell" => Ok(JobType::Shell),
            "agent" => Ok(JobType::Agent),
            _ => Err(format!(
                "Invalid job type '{}'. Expected one of: 'shell', 'agent'",
                value
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionTarget {
    #[default]
    Isolated,
    Main,
}

impl SessionTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Main => "main",
        }
    }

    pub fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("main") {
            Self::Main
        } else {
            Self::Isolated
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    At {
        at: DateTime<Utc>,
    },
    Every {
        every_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default = "default_true")]
    pub best_effort: bool,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: "none".to_string(),
            channel: None,
            to: None,
            thread_id: None,
            best_effort: true,
        }
    }
}

pub fn default_true() -> bool {
    true
}

fn default_source() -> String {
    "imperative".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub expression: String,
    pub schedule: Schedule,
    pub command: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub job_type: JobType,
    pub session_target: SessionTarget,
    pub model: Option<String>,
    /// Agent alias this job runs under. Empty when the row was written
    /// before the column existed and no agent has claimed it; the
    /// scheduler skips such rows with a warning rather than coercing
    /// them to a magic alias.
    #[serde(default)]
    pub agent_alias: String,
    pub enabled: bool,
    pub delivery: DeliveryConfig,
    pub delete_after_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Whether to recall and inject memory context before this agent job runs.
    /// Defaults to `true`; set to `false` for stateless digest jobs that should
    /// not accumulate or consume memory entries.
    #[serde(default = "default_true")]
    pub uses_memory: bool,
    /// How the job was created: `"imperative"` (CLI/API) or `"declarative"` (config).
    #[serde(default = "default_source")]
    pub source: String,
    /// Output format for shell jobs. `"wrapped"` (default) or `"raw"`.
    /// Declarative jobs read this from `CronJobDecl.shell_output_format` in
    /// the config; imperative jobs read it from the stored field in the DB.
    #[serde(default)]
    pub shell_output_format: CronShellOutputFormat,
    pub created_at: DateTime<Utc>,
    pub next_run: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub last_output: Option<String>,
    /// ADR-009 Phase 1: the tenant this job runs on behalf of, if any.
    /// `None` (the only value reachable via the public `cron_add` tool or
    /// `/api/cron` today) is today's exact behavior — an untenanted
    /// operator run. `Some` is only set by the internal
    /// `add_agent_job_for_tenant`, never by anything a tenant or agent can
    /// reach directly yet (that is Phase 2's job). Paired with
    /// `tenant_agent_type`: both `Some` or both `None`, never mixed — see
    /// [`CronJob::tenant_selector`].
    ///
    /// This is the **raw** platform user id, never the composed
    /// `<user_id>.<agent_type>` alias. `run_agent_job` hands it to
    /// `agent::tenant::resolve_tenant_context`, whose registered resolver
    /// assigns it to `TenantSelector::user_id` and derives the composed form
    /// itself. Storing the composed form resolves no persona row, and the
    /// job then refuses to run rather than failing anywhere visible.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// The Aivory agent type (`customer_service`, `leads_qualifier`, …)
    /// half of the tenant identity — see `tenant_id`.
    #[serde(default)]
    pub tenant_agent_type: Option<String>,
}

impl CronJob {
    /// `(tenant_id, agent_type)` if this job carries a complete tenant
    /// identity, `None` for an untenanted operator run — including the
    /// data-inconsistent case where only one of the two fields is set
    /// (never produced by `add_agent_job_for_tenant`, but a hand-edited
    /// row or a future migration bug should degrade to "no tenant" rather
    /// than resolving a tenant context from half an identity).
    pub fn tenant_selector(&self) -> Option<(&str, &str)> {
        match (self.tenant_id.as_deref(), self.tenant_agent_type.as_deref()) {
            (Some(id), Some(agent_type)) if !id.is_empty() && !agent_type.is_empty() => {
                Some((id, agent_type))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRun {
    pub id: i64,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String,
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronJobPatch {
    pub schedule: Option<Schedule>,
    pub command: Option<String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub delivery: Option<DeliveryConfig>,
    pub model: Option<String>,
    pub session_target: Option<SessionTarget>,
    pub delete_after_run: Option<bool>,
    pub allowed_tools: Option<Vec<String>>,
    pub uses_memory: Option<bool>,
    pub shell_output_format: Option<CronShellOutputFormat>,
}

impl ::zeroclaw_api::attribution::Attributable for CronJob {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        let kind = match self.schedule {
            Schedule::Cron { .. } => ::zeroclaw_api::attribution::CronKind::Cron,
            Schedule::At { .. } => ::zeroclaw_api::attribution::CronKind::At,
            Schedule::Every { .. } => ::zeroclaw_api::attribution::CronKind::Interval,
        };
        ::zeroclaw_api::attribution::Role::Cron(kind)
    }
    fn alias(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_job() -> CronJob {
        CronJob {
            id: "job-1".into(),
            expression: String::new(),
            schedule: Schedule::Every { every_ms: 60_000 },
            command: String::new(),
            prompt: None,
            name: None,
            job_type: JobType::Shell,
            session_target: SessionTarget::Isolated,
            model: None,
            agent_alias: "test-agent".into(),
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            allowed_tools: None,
            uses_memory: true,
            source: "imperative".into(),
            shell_output_format: CronShellOutputFormat::default(),
            created_at: chrono::Utc::now(),
            next_run: chrono::Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
            tenant_id: None,
            tenant_agent_type: None,
        }
    }

    #[test]
    fn tenant_selector_is_none_by_default() {
        assert!(minimal_job().tenant_selector().is_none());
    }

    #[test]
    fn tenant_selector_is_some_when_both_fields_set() {
        let mut job = minimal_job();
        job.tenant_id = Some("u1".into());
        job.tenant_agent_type = Some("customer_service".into());
        assert_eq!(
            job.tenant_selector(),
            Some(("u1", "customer_service"))
        );
    }

    #[test]
    fn tenant_selector_is_none_when_only_one_field_set() {
        let mut job = minimal_job();
        job.tenant_id = Some("u1".into());
        job.tenant_agent_type = None;
        assert!(
            job.tenant_selector().is_none(),
            "half an identity must never resolve a tenant scope"
        );

        let mut job = minimal_job();
        job.tenant_id = None;
        job.tenant_agent_type = Some("customer_service".into());
        assert!(job.tenant_selector().is_none());
    }

    #[test]
    fn tenant_selector_is_none_when_either_field_is_empty_string() {
        let mut job = minimal_job();
        job.tenant_id = Some(String::new());
        job.tenant_agent_type = Some("customer_service".into());
        assert!(job.tenant_selector().is_none());
    }

    #[test]
    fn deserialize_schedule_from_object() {
        let val = serde_json::json!({"kind": "cron", "expr": "*/5 * * * *"});
        let sched = deserialize_maybe_stringified::<Schedule>(&val).unwrap();
        assert!(matches!(sched, Schedule::Cron { ref expr, .. } if expr == "*/5 * * * *"));
    }

    #[test]
    fn deserialize_schedule_from_string() {
        let val = serde_json::Value::String(r#"{"kind":"cron","expr":"*/5 * * * *"}"#.to_string());
        let sched = deserialize_maybe_stringified::<Schedule>(&val).unwrap();
        assert!(matches!(sched, Schedule::Cron { ref expr, .. } if expr == "*/5 * * * *"));
    }

    #[test]
    fn deserialize_schedule_string_with_tz() {
        let val = serde_json::Value::String(
            r#"{"kind":"cron","expr":"*/30 9-15 * * 1-5","tz":"Asia/Shanghai"}"#.to_string(),
        );
        let sched = deserialize_maybe_stringified::<Schedule>(&val).unwrap();
        match sched {
            Schedule::Cron { tz, .. } => assert_eq!(tz.as_deref(), Some("Asia/Shanghai")),
            _ => panic!("expected Cron variant"),
        }
    }

    #[test]
    fn deserialize_every_from_string() {
        let val = serde_json::Value::String(r#"{"kind":"every","every_ms":60000}"#.to_string());
        let sched = deserialize_maybe_stringified::<Schedule>(&val).unwrap();
        assert!(matches!(sched, Schedule::Every { every_ms: 60000 }));
    }

    #[test]
    fn deserialize_invalid_string_returns_error() {
        let val = serde_json::Value::String("not json at all".to_string());
        assert!(deserialize_maybe_stringified::<Schedule>(&val).is_err());
    }

    #[test]
    fn job_type_try_from_accepts_known_values_case_insensitive() {
        assert_eq!(JobType::try_from("shell").unwrap(), JobType::Shell);
        assert_eq!(JobType::try_from("SHELL").unwrap(), JobType::Shell);
        assert_eq!(JobType::try_from("agent").unwrap(), JobType::Agent);
        assert_eq!(JobType::try_from("AgEnT").unwrap(), JobType::Agent);
    }

    #[test]
    fn job_type_try_from_rejects_invalid_values() {
        assert!(JobType::try_from("").is_err());
        assert!(JobType::try_from("unknown").is_err());
    }
}
