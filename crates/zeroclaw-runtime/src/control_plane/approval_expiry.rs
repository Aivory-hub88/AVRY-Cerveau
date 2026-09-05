//! ADR-009 §Decision-5, the last piece — retiring an unattended approval
//! that nobody ever answered.
//!
//! A live approval is self-limiting: a person is sitting in front of it, and
//! either they decide or they close the tab and the question dies with the
//! conversation. An approval raised by a scheduled run has neither. It was
//! created at 03:00 by a job nobody watched, and unless something retires it
//! it stays `pending` forever — so a weekly schedule that keeps hitting the
//! same `Irreversible` tool quietly grows a pile, and the pile is indexed by
//! nothing and read by no one.
//!
//! **Expiry is not denial.** A denial is a decision a person made, and the
//! caller is owed a reply — which is why `resolve` feeds
//! `sweep_undelivered_approvals` and buys a continuation turn. An expiry is
//! the *absence* of a decision, on a run nobody was watching. There is no
//! one to reply to, so paying for a turn to say "nobody answered" would be
//! spend with no reader. The `expired` status exists to keep those two
//! apart, and the redelivery sweep's `status IN ('approved','denied')`
//! filter is what makes the separation load-bearing rather than cosmetic.
//!
//! **Only unattended rows are touched.** An attended approval is somebody's
//! open question; retiring it out from under them would be this module
//! deciding something it has no standing to decide. Unattended is read
//! through [`crate::cron::scheduler::is_unattended_session`] — the single
//! reader of the session prefix, per ADR-009 Phase 3 — rather than a second
//! `LIKE 'cron-%'` in SQL that could drift away from it silently.
//!
//! Nothing here approves anything, ever. The only transition it can perform
//! is `pending → expired`, and it loses every race against a human: the
//! `UPDATE` is guarded on `status = 'pending'`, so a decision made moments
//! before a tick stands.
//!
//! **ADR-009 §14 follow-up.** Retiring the row closes it out of the
//! Notification Centre, but that is the *only* place it was ever visible —
//! so from the tenant's side, a question their schedule raised simply
//! vanished with no trace anywhere. A lapsed approval whose
//! `pending_approvals.schedule_id` names a `product.tenant_scheduled_runs`
//! row is now reported back to avry-backend, best-effort, over the same
//! seam `cron::tenant_sync` already uses. An install with no backend
//! configured (or one where the row was never schedule-originated) simply
//! has nothing to report — expiring the row itself never depends on this.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::pending_approvals::{PendingApproval, PendingApprovalsStore};

/// Timeout on the best-effort report to avry-backend. Short on purpose: a
/// sweep tick runs every 15 minutes regardless, so a slow or hung backend
/// should not hold this loop open — the fifteen-minute clock, not a long
/// per-call timeout, is what makes a missed report harmless.
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an unattended approval waits for an answer.
///
/// 72 hours is chosen to survive a weekend: a job that fires late on Friday
/// and raises a question still has it waiting when someone reads their
/// notifications on Monday morning. Much longer and the tool call stops
/// being connected to the situation that produced it — approving on Thursday
/// an email a Sunday-night run wanted to send is its own kind of wrong, and
/// the safer failure is that it lapsed rather than that it was still there
/// to be clicked.
pub const EXPIRY_AFTER: chrono::Duration = chrono::Duration::hours(72);

/// How often the sweep looks. A three-day deadline does not need a fast
/// clock, and this runs on every instance forever; a quarter-hour is far
/// more resolution than the decision needs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Run the sweep until `cancel` fires.
pub async fn sweep_loop(store: Arc<PendingApprovalsStore>, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Built once, not per tick: a report is best-effort and rare (a lapse
    // requires 72 unanswered hours), so there is no traffic volume here to
    // justify a fresh client every 15 minutes.
    let client = reqwest::Client::builder()
        .timeout(REPORT_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => sweep_once(&store, &client, Utc::now()).await,
        }
    }
}

/// One pass. `now` is a parameter so a test can age rows without sleeping.
async fn sweep_once(store: &PendingApprovalsStore, client: &reqwest::Client, now: DateTime<Utc>) {
    let pending = match store.list(Some("pending")) {
        Ok(rows) => rows,
        Err(e) => {
            // A store read failing is not worth escalating: the next tick
            // recomputes the same set, and there is nothing a caller could
            // do with the error that waiting fifteen minutes does not.
            note("could not list pending approvals", &format!("{e:#}"));
            return;
        }
    };

    for row in pending.iter().filter(|row| is_expirable(row, now)) {
        match store.expire(&row.id) {
            Ok(expired) => {
                // `false` is the race this is designed to lose — someone
                // resolved the row between the list and the update. Not an
                // error, and nothing happened here worth a line or a report.
                if let Some(schedule_id) = lapsed_report_target(expired, row.schedule_id.as_deref())
                {
                    note_expired(row);
                    report_lapsed_schedule(client, schedule_id, &row.tool_name).await;
                } else if expired {
                    note_expired(row);
                }
            }
            Err(e) => note("could not expire an approval", &format!("{e:#}")),
        }
    }
}

/// Whether an expiry is worth reporting to avry-backend, and what to send —
/// pulled out so the decision is testable without a mock HTTP server. `None`
/// on the race-loss case (`expired = false`) and on a row this sweep expired
/// but that was never raised by a schedule (`schedule_id = None`) — an
/// operator's own cron job, or a live turn's approval that happened to sit
/// unanswered past the window (session prefix aside, only `Isolated`
/// tenant-schedule jobs ever populate `schedule_id`).
fn lapsed_report_target(expired: bool, schedule_id: Option<&str>) -> Option<&str> {
    if !expired {
        return None;
    }
    schedule_id
}

/// Best-effort: tell avry-backend that a schedule's last run raised a
/// question nobody answered. Silent no-op when the backend seam is not
/// configured — the same install has nothing to reconcile against either.
async fn report_lapsed_schedule(client: &reqwest::Client, schedule_id: &str, tool_name: &str) {
    let Some((base, token)) = crate::cron::tenant_sync::backend() else {
        return;
    };
    let body = ::serde_json::json!({ "tool_name": tool_name });
    let res = client
        .post(format!(
            "{base}/api/v1/tenant-scheduled-runs/internal/{schedule_id}/lapsed-approval"
        ))
        .header("X-Internal-Token", token)
        .json(&body)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        // A 404 here means the schedule was deleted after it fired and
        // before this sweep caught the lapse — expected, not a fault.
        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {}
        Ok(r) => note(
            "lapsed-approval report rejected by avry-backend",
            &format!("HTTP {}", r.status()),
        ),
        Err(e) => note(
            "could not report a lapsed approval to its schedule",
            &format!("{e}"),
        ),
    }
}

/// Whether this row is an unanswered unattended approval old enough to
/// retire. A row whose `requested_at` will not parse is deliberately left
/// alone: the one irreversible thing here is retiring something, and doing
/// it on an unreadable timestamp would be guessing.
fn is_expirable(row: &PendingApproval, now: DateTime<Utc>) -> bool {
    if row.status != "pending" {
        return false;
    }
    if !crate::cron::scheduler::is_unattended_session(row.session_id.as_deref()) {
        return false;
    }
    DateTime::parse_from_rfc3339(&row.requested_at)
        .map(|requested| now.signed_duration_since(requested.with_timezone(&Utc)) >= EXPIRY_AFTER)
        .unwrap_or(false)
}

fn note_expired(row: &PendingApproval) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "approval_id": row.id,
                "tool": row.tool_name,
                "requested_at": row.requested_at,
                "tenant_id": row.tenant_id,
            })
        ),
        "unattended approval expired unanswered"
    );
}

fn note(message: &str, detail: &str) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "detail": detail })),
        &format!("approval expiry: {message}")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::scheduler::CRON_SESSION_PREFIX;

    /// A client that will never actually be used for a network call in this
    /// test binary: `AVRY_BACKEND_INTERNAL_URL` is never set in a test run,
    /// so `report_lapsed_schedule` returns before touching it — same
    /// no-env-mutation discipline `cron::tenant_sync`'s own tests follow.
    fn test_client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn ago(hours: i64) -> String {
        (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
    }

    /// Insert a pending row of a chosen age, session and schedule id, and
    /// hand back its id — so a test can set up the three things this module
    /// actually branches on without going through a whole turn.
    fn insert_full(
        store: &PendingApprovalsStore,
        session: Option<&str>,
        requested_at: &str,
        schedule_id: Option<&str>,
    ) -> String {
        let id = store
            .insert_with_context(
                "tenant-1",
                "GMAIL__SEND_EMAIL",
                "{}",
                "irreversible",
                Some("tenant-1.customer_service"),
                Some("customer_service"),
                session,
                Some("Summarise last week and email ops."),
                schedule_id,
            )
            .unwrap();
        store.backdate_for_test(&id, requested_at);
        id
    }

    fn insert(store: &PendingApprovalsStore, session: Option<&str>, requested_at: &str) -> String {
        insert_full(store, session, requested_at, None)
    }

    fn unattended(store: &PendingApprovalsStore, requested_at: &str) -> String {
        insert(
            store,
            Some(&format!("{CRON_SESSION_PREFIX}abc")),
            requested_at,
        )
    }

    fn status_of(store: &PendingApprovalsStore, id: &str) -> String {
        store.get(id).unwrap().unwrap().status
    }

    #[tokio::test]
    async fn an_old_unattended_approval_is_retired() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(73));

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(status_of(&store, &id), "expired");
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(
            row.resolved_by.as_deref(),
            Some(PendingApprovalsStore::EXPIRY_ACTOR),
            "an audit read must be able to tell a lapse from an unknown resolver"
        );
        assert!(row.resolved_at.is_some());
    }

    #[tokio::test]
    async fn an_attended_approval_is_never_retired_however_old() {
        // Somebody's open question. Age is not this module's business.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let live = insert(&store, Some("sess-42"), &ago(24 * 365));
        let no_session = insert(&store, None, &ago(24 * 365));

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(status_of(&store, &live), "pending");
        assert_eq!(status_of(&store, &no_session), "pending");
    }

    #[tokio::test]
    async fn an_unattended_approval_inside_the_window_still_waits() {
        // The whole point of 72h is that a Friday-night run survives until
        // Monday. 71 hours in, the answer is still wanted.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(71));

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(status_of(&store, &id), "pending");
    }

    #[tokio::test]
    async fn expiry_never_touches_an_already_decided_row() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(500));
        store.resolve(&id, "approved", "ops@example.com").unwrap();

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(
            status_of(&store, &id),
            "approved",
            "a decision that was actually made must outlive any sweep"
        );
    }

    #[tokio::test]
    async fn an_unparseable_timestamp_is_left_alone() {
        // Retiring is the irreversible move here; doing it off a timestamp
        // we cannot read would be guessing.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, "not-a-timestamp");

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(status_of(&store, &id), "pending");
    }

    #[tokio::test]
    async fn an_expired_row_never_reaches_the_redelivery_sweep() {
        // This is what makes "expiry is not denial" real rather than a
        // naming choice: `list_undelivered_resolved` is what buys a
        // continuation turn, and an expiry has nobody to deliver to.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let lapsed = unattended(&store, &ago(500));
        let denied = unattended(&store, &ago(500));
        store.resolve(&denied, "denied", "ops@example.com").unwrap();

        sweep_once(&store, &test_client(), Utc::now()).await;

        let ids: Vec<String> = store
            .list_undelivered_resolved()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(
            ids.contains(&denied),
            "a real denial still owes its caller a reply"
        );
        assert!(
            !ids.contains(&lapsed),
            "an expiry must not buy a continuation turn for a reader who does not exist"
        );
    }

    #[tokio::test]
    async fn an_expired_row_drops_out_of_the_pending_list() {
        // The Notification Centre asks for `status=pending`; this is the
        // whole user-visible effect of the sweep.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let _old = unattended(&store, &ago(500));
        let live = insert(&store, Some("sess-42"), &ago(500));

        sweep_once(&store, &test_client(), Utc::now()).await;

        let pending: Vec<String> = store
            .list(Some("pending"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(pending, vec![live]);
    }

    #[test]
    fn lapsed_report_target_only_fires_on_a_real_expiry_with_a_schedule() {
        // The exact bug this replaced: the old match arm was `Ok(_) =>
        // note_expired(row)`, which logged "expired" even on the race-loss
        // case (`Ok(false)`) despite a comment saying that case was "not
        // worth a line". Pinning the decision as a pure function makes that
        // kind of drift between the comment and the code impossible.
        assert_eq!(lapsed_report_target(true, Some("sched-1")), Some("sched-1"));
        assert_eq!(lapsed_report_target(false, Some("sched-1")), None);
        assert_eq!(lapsed_report_target(true, None), None);
        assert_eq!(lapsed_report_target(false, None), None);
    }

    #[tokio::test]
    async fn a_schedule_originated_lapse_is_still_retired_with_no_backend_configured() {
        // The reporting half must never gate the retiring half — an install
        // with no avry-backend has nothing to report to, but the row still
        // needs to stop blocking the Notification Centre.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = insert_full(
            &store,
            Some(&format!("{CRON_SESSION_PREFIX}abc")),
            &ago(500),
            Some("schedule-xyz"),
        );

        sweep_once(&store, &test_client(), Utc::now()).await;

        assert_eq!(status_of(&store, &id), "expired");
    }
}
