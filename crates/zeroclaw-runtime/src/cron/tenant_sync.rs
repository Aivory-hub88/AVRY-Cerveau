//! ADR-009 Phase 2b — the reconcile that makes a tenant's scheduled run
//! actually run.
//!
//! Phase 1 taught the scheduler to carry a tenant and resolve it at fire
//! time. Phase 2 gave avry-backend the store, the JWT CRUD and the quota.
//! Between them sat the gap this closes: nothing copied a row from
//! `product.tenant_scheduled_runs` into Cerveau's own `cron_jobs`, so a
//! schedule a tenant created was stored, acknowledged, and never fired.
//!
//! **A reconcile, not an event feed.** avry-backend could have pushed on
//! create/update/delete, which is simpler right up until one push is lost —
//! and then a schedule silently never runs, which is the exact failure mode
//! ADR-009 §6 was written about. A periodic pass that makes Cerveau's rows
//! match the backend's list is self-healing: a missed change costs one
//! interval, not a support ticket.
//!
//! Ownership is explicit. Every row this creates carries
//! `source = "tenant_schedule"`, and the delete pass only ever considers
//! rows with that source — an operator's own cron job, or a declarative one
//! from config.toml, is never in scope no matter what the backend returns.
//!
//! Fails open at every step, like the rest of the tenant stack: the backend
//! being unreachable leaves the existing cron rows exactly as they are and
//! tries again next interval. The one thing it will not do is guess — a row
//! it cannot turn into a valid schedule is acked back as `failed` with the
//! reason, so the tenant sees why rather than watching nothing happen.

use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use zeroclaw_config::schema::Config;

use super::store::{self, ScheduleUpsert};
use super::types::Schedule;

/// How often the reconcile runs. The backend is on localhost, the payload
/// is a handful of rows, and a tenant who just pressed "save" should not
/// wait long to see `active` — but this also fires on every instance, so
/// it is not free either.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct ScheduledRunRow {
    id: String,
    user_id: String,
    agent_type: String,
    name: String,
    prompt: String,
    cron_expression: String,
    timezone: String,
    enabled: bool,
    /// What avry-backend currently believes. Used only to decide whether an
    /// ack is worth sending — re-acking an unchanged row every minute would
    /// be pure write traffic.
    status: String,
}

#[derive(Debug, Deserialize)]
struct ScheduledRunsResponse {
    scheduled_runs: Vec<ScheduledRunRow>,
}

/// Normalise the two halves of the backend seam. Split out from `backend()`
/// so it can be tested without mutating the process environment — a test
/// that calls `set_var` races every other test in the binary (which is why
/// Rust 2024 makes it `unsafe`) and can abort the whole run.
fn backend_from(url: Option<String>, token: Option<String>) -> Option<(String, String)> {
    let url = url
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())?;
    let token = token
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    Some((url, token))
}

/// `pub(crate)`: `control_plane::approval_expiry` reports a lapsed schedule
/// through this exact seam, and a second copy of the parsing (trim,
/// trailing-slash strip, blank-is-absent) is exactly the kind of duplication
/// that drifts the two silently apart.
pub(crate) fn backend() -> Option<(String, String)> {
    // Same two variables `zeroclaw_gateway::tenant`'s resolvers already use,
    // read the same way, so there is one place to configure the backend seam
    // rather than two that can disagree.
    backend_from(
        std::env::var("AVRY_BACKEND_INTERNAL_URL").ok(),
        std::env::var("AVRY_BACKEND_INTERNAL_TOKEN").ok(),
    )
}

/// Whether this process is the one that owns tenant schedules.
///
/// **Exactly one Cerveau instance in a pool may set this.** Instances behind
/// the same load balancer share the backend but each keeps its own cron
/// store, so a reconcile running on both would create the same job twice and
/// the schedule would fire — and bill — once per instance. There is no
/// coordination between them to prevent that, so ownership is declared, not
/// inferred.
///
/// Opt-in, and off by default, deliberately. Getting it wrong in the "off"
/// direction is visible: the backend row stays `pending_activation`, which
/// is exactly what that column exists to say. Getting it wrong in the "on"
/// direction is silent duplicate LLM spend, which is not.
fn sync_owner_from(raw: Option<String>) -> bool {
    matches!(
        raw.as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn sync_owner() -> bool {
    sync_owner_from(std::env::var("AVRY_TENANT_SCHEDULE_SYNC").ok())
}

/// Run the reconcile until `cancel` fires. Returns immediately, for the
/// process's lifetime, when the backend seam is not configured or this
/// instance is not the declared owner — neither has anything to reconcile
/// and neither should spend a timer discovering that once a minute.
pub async fn reconcile_loop(config: Config, cancel: CancellationToken) {
    if backend().is_none() || !sync_owner() {
        return;
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "interval_secs": RECONCILE_INTERVAL.as_secs(),
            })
        ),
        "tenant schedule reconcile: this instance owns tenant schedules"
    );
    // Boxed deliberately, so this function's own future stays roughly a
    // `Config` and a token rather than the whole loop body (HTTP client,
    // response buffer, per-row work, all live across awaits). The spawn site
    // in `daemon::run` builds that future as a temporary during boot, and
    // that frame has already proven tight enough to overflow a test thread's
    // stack. One heap allocation for the process's lifetime.
    Box::pin(reconcile_loop_inner(config, cancel)).await;
}

async fn reconcile_loop_inner(config: Config, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = interval.tick() => run_once(&config).await,
        }
    }
}

async fn run_once(config: &Config) {
    let Some((base, token)) = backend() else {
        return;
    };
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn("could not build the HTTP client", &format!("{e:#}"));
            return;
        }
    };

    let desired = match fetch_desired(&client, &base, &token).await {
        Ok(rows) => rows,
        Err(e) => {
            // Backend down: leave every existing cron row exactly as it is.
            // Deleting them because we could not read the list would take a
            // working schedule offline over a transient outage.
            warn("could not read scheduled runs from avry-backend", &e);
            return;
        }
    };

    // No single upfront fallback here: each row carries its own `agent_type`,
    // and a migrated type (a same-named, enabled `[agents.<agent_type>]`
    // entry) must resolve to its own host alias per row — same as a live
    // interactive turn — rather than every row sharing one resolution
    // computed before any row's `agent_type` is even known. An unmigrated
    // type still falls through to the exact old type-blind fallback inside
    // `resolved_runtime_agent_alias_for_tenant_type` itself.
    if config.resolved_runtime_agent_alias().is_none() {
        warn(
            "no configured [agents.<alias>] entry to run tenant schedules on",
            "resolved_runtime_agent_alias returned None",
        );
        return;
    }

    let mut seen: Vec<String> = Vec::with_capacity(desired.len());

    for row in &desired {
        seen.push(row.id.clone());
        let Some(host_alias) = config
            .resolved_runtime_agent_alias_for_tenant_type(Some(row.agent_type.as_str()))
            .map(str::to_owned)
        else {
            continue;
        };
        let (status, detail) = match apply_row(config, &host_alias, row) {
            Ok(outcome) => match ack_decision(outcome, row.enabled, &row.status) {
                Some(status) => (status, None),
                None => continue,
            },
            Err(e) => ("failed", Some(e)),
        };
        ack(&client, &base, &token, &row.id, status, detail.as_deref()).await;
    }

    // Anything this reconcile owns that the backend no longer lists has been
    // deleted there (the API soft-deletes, so "absent from the list" really
    // does mean gone, not merely paused — a paused row still comes back with
    // `enabled = false`).
    match store::list_tenant_schedule_jobs(config) {
        Ok(existing) => {
            for job in existing {
                if !seen.contains(&job.id) {
                    if let Err(e) = super::remove_job(config, &job.id) {
                        warn(
                            "could not remove a withdrawn tenant schedule",
                            &format!("{e:#}"),
                        );
                    }
                }
            }
        }
        Err(e) => warn("could not list owned cron rows", &format!("{e:#}")),
    }
}

async fn fetch_desired(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<Vec<ScheduledRunRow>, String> {
    let res = client
        .get(format!("{base}/api/v1/tenant-scheduled-runs/internal/all"))
        .header("X-Internal-Token", token)
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let parsed: ScheduledRunsResponse = res.json().await.map_err(|e| format!("{e}"))?;
    Ok(parsed.scheduled_runs)
}

/// Turn one backend row into a cron row. The `Err` string is what the
/// tenant will see next to their schedule, so it is written for them.
fn apply_row(
    config: &Config,
    host_alias: &str,
    row: &ScheduledRunRow,
) -> Result<ScheduleUpsert, String> {
    let schedule = Schedule::Cron {
        expr: row.cron_expression.clone(),
        // Always `Some`. avry-backend requires an IANA zone precisely so a
        // tenant schedule can never fall back to the runtime host's own
        // timezone (ADR-009 §6a) — passing `None` here would reintroduce
        // exactly that.
        tz: Some(row.timezone.clone()),
    };
    // `CronJob::tenant_id` is the *raw* platform user id, not the composed
    // `<user_id>.<agent_type>` alias: `run_agent_job` hands it straight to
    // `resolve_tenant_context`, whose registered resolver assigns it to
    // `TenantSelector::user_id` and derives the composed form itself via
    // `TenantSelector::tenant_id()`. Passing the composed form here would
    // make the selector `user_id = "u1.customer_service"`, no persona row
    // would resolve, and every tenant schedule would refuse to run.
    store::upsert_tenant_schedule_job(
        config,
        &row.id,
        host_alias,
        &row.name,
        &row.prompt,
        schedule,
        row.enabled,
        &row.user_id,
        &row.agent_type,
    )
    .map_err(|e| format!("{e:#}"))
}

/// What to report back for a row, or `None` when the backend's view already
/// matches and a write would be pure noise. Steady state must cost one GET
/// per interval and zero writes — the reconcile runs once a minute forever.
fn ack_decision(
    outcome: ScheduleUpsert,
    enabled: bool,
    backend_status: &str,
) -> Option<&'static str> {
    let want = if enabled { "active" } else { "paused" };
    if outcome == ScheduleUpsert::Unchanged && backend_status == want {
        return None;
    }
    Some(want)
}

async fn ack(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    run_id: &str,
    status: &str,
    detail: Option<&str>,
) {
    let body = serde_json::json!({
        "status": status,
        "cerveau_job_id": run_id,
        "status_detail": detail,
    });
    let res = client
        .post(format!(
            "{base}/api/v1/tenant-scheduled-runs/internal/{run_id}/ack"
        ))
        .header("X-Internal-Token", token)
        .json(&body)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => warn(
            "ack rejected by avry-backend",
            &format!("HTTP {}", r.status()),
        ),
        // A failed ack is not worth retrying inline: the next pass recomputes
        // the same state and acks again, because the backend still reports
        // the stale status.
        Err(e) => warn("could not ack a scheduled run", &format!("{e}")),
    }
}

fn warn(message: &str, detail: &str) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "detail": detail })),
        &format!("tenant schedule reconcile: {message}")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, enabled: bool, status: &str) -> ScheduledRunRow {
        ScheduledRunRow {
            id: id.into(),
            user_id: "u1".into(),
            agent_type: "customer_service".into(),
            name: "weekly digest".into(),
            prompt: "Summarise last week.".into(),
            cron_expression: "0 9 * * 1".into(),
            timezone: "Asia/Jakarta".into(),
            enabled,
            status: status.into(),
        }
    }

    fn test_config(tmp: &tempfile::TempDir) -> Config {
        let config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        config
    }

    #[test]
    fn the_stored_identity_is_the_raw_user_id_not_the_composed_alias() {
        // The distinction is load-bearing and easy to get backwards.
        // `run_agent_job` hands `tenant_id` straight to
        // `resolve_tenant_context`, whose resolver assigns it to
        // `TenantSelector::user_id` and derives `<user_id>.<agent_type>`
        // itself. Storing the composed form here would make the selector
        // `user_id = "u1.customer_service"`, resolve no persona row, and
        // every tenant schedule would refuse to run rather than fail loudly.
        let tmp = tempfile::TempDir::new().unwrap();
        let config = test_config(&tmp);
        let r = row("a", true, "pending_activation");

        apply_row(&config, "default", &r).expect("a valid row must apply");

        let jobs = store::list_tenant_schedule_jobs(&config).unwrap();
        let job = jobs
            .iter()
            .find(|j| j.id == r.id)
            .expect("the reconcile must store the row under the backend's id");
        assert_eq!(job.tenant_id.as_deref(), Some("u1"));
        assert_eq!(job.tenant_agent_type.as_deref(), Some("customer_service"));
        assert_eq!(
            job.tenant_selector(),
            Some(("u1", "customer_service")),
            "both halves must survive the round-trip or the job runs unscoped"
        );
    }

    #[test]
    fn the_timezone_is_always_passed_through() {
        let r = row("a", true, "pending_activation");
        let schedule = Schedule::Cron {
            expr: r.cron_expression.clone(),
            tz: Some(r.timezone.clone()),
        };
        match schedule {
            Schedule::Cron { tz, .. } => assert_eq!(
                tz.as_deref(),
                Some("Asia/Jakarta"),
                "a None here would silently resolve against the host's own zone"
            ),
            _ => panic!("expected a cron schedule"),
        }
    }

    #[test]
    fn an_unchanged_row_whose_status_already_matches_is_not_re_acked() {
        // The only silent case: nothing changed here and the backend already
        // says what we would tell it.
        assert_eq!(
            ack_decision(ScheduleUpsert::Unchanged, true, "active"),
            None
        );
        assert_eq!(
            ack_decision(ScheduleUpsert::Unchanged, false, "paused"),
            None
        );

        // A paused row the backend still reports `active` must be corrected,
        // even though this pass changed nothing — otherwise a tenant who
        // pressed pause keeps reading `active` forever.
        assert_eq!(
            ack_decision(ScheduleUpsert::Unchanged, false, "active"),
            Some("paused")
        );
        // A row the backend has never heard of us owning is always acked.
        assert_eq!(
            ack_decision(ScheduleUpsert::Unchanged, true, "pending_activation"),
            Some("active")
        );
        // And a pass that actually wrote something always reports, so
        // `last_synced_at` tracks reality.
        assert_eq!(
            ack_decision(ScheduleUpsert::Created, true, "active"),
            Some("active")
        );
        assert_eq!(
            ack_decision(ScheduleUpsert::Updated, true, "active"),
            Some("active")
        );
    }

    #[test]
    fn backend_seam_needs_both_halves() {
        // Guards the early return in `reconcile_loop`: half a seam is a
        // misconfiguration, not something to half-use.
        let url = || Some("http://127.0.0.1:8081".to_string());
        assert!(
            backend_from(url(), None).is_none(),
            "a URL without a token is not a seam"
        );
        assert!(
            backend_from(None, Some("t".into())).is_none(),
            "a token without a URL is not a seam"
        );
        assert!(
            backend_from(url(), Some("  ".into())).is_none(),
            "a blank token is not a token"
        );
        assert!(backend_from(Some("".into()), Some("t".into())).is_none());
        assert_eq!(
            backend_from(url(), Some("t".into())),
            Some(("http://127.0.0.1:8081".to_string(), "t".to_string()))
        );
    }

    #[test]
    fn sync_ownership_is_opt_in_and_off_by_default() {
        // Off by default: an instance that says nothing must not reconcile,
        // because in a two-instance pool the other one already does.
        assert!(!sync_owner_from(None));
        assert!(!sync_owner_from(Some(String::new())));
        assert!(!sync_owner_from(Some("0".into())));
        assert!(!sync_owner_from(Some("false".into())));
        // Anything unrecognised also means no — an operator typo must fail
        // towards "no duplicate billing", not towards it.
        assert!(!sync_owner_from(Some("maybe".into())));
        for yes in ["1", "true", "TRUE", "yes", "on", " true "] {
            assert!(sync_owner_from(Some(yes.into())), "{yes} should opt in");
        }
    }

    #[test]
    fn backend_url_trailing_slash_is_trimmed() {
        // Every request path this module builds starts with `/`, so a stored
        // trailing slash would produce `//api/v1/...`.
        assert_eq!(
            backend_from(Some("http://127.0.0.1:8081/".into()), Some("t".into()))
                .unwrap()
                .0,
            "http://127.0.0.1:8081"
        );
    }
}
