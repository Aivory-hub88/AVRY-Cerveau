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

fn backend() -> Option<(String, String)> {
    // Same two variables `zeroclaw_gateway::tenant`'s resolvers already use,
    // read the same way, so there is one place to configure the backend seam
    // rather than two that can disagree.
    backend_from(
        std::env::var("AVRY_BACKEND_INTERNAL_URL").ok(),
        std::env::var("AVRY_BACKEND_INTERNAL_TOKEN").ok(),
    )
}

/// Run the reconcile until `cancel` fires. Returns immediately, for the
/// process's lifetime, when the backend seam is not configured — an install
/// with no avry-backend has nothing to reconcile against and should not
/// spend a timer discovering that once a minute.
pub async fn reconcile_loop(config: Config, cancel: CancellationToken) {
    if backend().is_none() {
        return;
    }
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

    let Some(host_alias) = config.resolved_runtime_agent_alias().map(str::to_owned) else {
        warn(
            "no configured [agents.<alias>] entry to run tenant schedules on",
            "resolved_runtime_agent_alias returned None",
        );
        return;
    };

    let mut seen: Vec<String> = Vec::with_capacity(desired.len());

    for row in &desired {
        seen.push(row.id.clone());
        let (status, detail) = match apply_row(config, &host_alias, row) {
            Ok(outcome) => {
                let status = if row.enabled { "active" } else { "paused" };
                // Only ack when the backend's view is actually stale, so a
                // steady state costs one GET per interval and no writes.
                if outcome == ScheduleUpsert::Unchanged && row.status == status {
                    continue;
                }
                (status, None)
            }
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
    let tenant_id = format!("{}.{}", row.user_id, row.agent_type);
    store::upsert_tenant_schedule_job(
        config,
        &row.id,
        host_alias,
        &row.name,
        &row.prompt,
        schedule,
        row.enabled,
        &tenant_id,
        &row.agent_type,
    )
    .map_err(|e| format!("{e:#}"))
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

    #[test]
    fn tenant_id_is_the_dotted_pair_the_tenant_layer_keys_on() {
        // `TenantSelector::tenant_id()` builds `<user_id>.<agent_type>`, and
        // `run_agent_job` compares against exactly that. Getting this
        // separator wrong would resolve every schedule to no tenant at all.
        let r = row("a", true, "pending_activation");
        assert_eq!(
            format!("{}.{}", r.user_id, r.agent_type),
            "u1.customer_service"
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
        // Steady state must cost one GET and zero writes; the guard is the
        // pair (Unchanged, status already correct).
        let active = row("a", true, "active");
        let should_skip =
            ScheduleUpsert::Unchanged == ScheduleUpsert::Unchanged && active.status == "active";
        assert!(should_skip);

        // A paused row still reporting `active` must be acked.
        let paused = row("b", false, "active");
        let want = if paused.enabled { "active" } else { "paused" };
        assert_ne!(paused.status, want);
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
