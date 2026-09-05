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

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::pending_approvals::{PendingApproval, PendingApprovalsStore};

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
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => sweep_once(&store, Utc::now()),
        }
    }
}

/// One pass. `now` is a parameter so a test can age rows without sleeping.
fn sweep_once(store: &PendingApprovalsStore, now: DateTime<Utc>) {
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
            // `false` is the race this is designed to lose — someone
            // resolved the row between the list and the update. Not an
            // error, and not worth a line.
            Ok(_) => note_expired(row),
            Err(e) => note("could not expire an approval", &format!("{e:#}")),
        }
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

    fn ago(hours: i64) -> String {
        (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
    }

    /// Insert a pending row of a chosen age and session, and hand back its
    /// id — so a test can set up the two things this module actually
    /// branches on without going through a whole turn.
    fn insert(store: &PendingApprovalsStore, session: Option<&str>, requested_at: &str) -> String {
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
            )
            .unwrap();
        store.backdate_for_test(&id, requested_at);
        id
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

    #[test]
    fn an_old_unattended_approval_is_retired() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(73));

        sweep_once(&store, Utc::now());

        assert_eq!(status_of(&store, &id), "expired");
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(
            row.resolved_by.as_deref(),
            Some(PendingApprovalsStore::EXPIRY_ACTOR),
            "an audit read must be able to tell a lapse from an unknown resolver"
        );
        assert!(row.resolved_at.is_some());
    }

    #[test]
    fn an_attended_approval_is_never_retired_however_old() {
        // Somebody's open question. Age is not this module's business.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let live = insert(&store, Some("sess-42"), &ago(24 * 365));
        let no_session = insert(&store, None, &ago(24 * 365));

        sweep_once(&store, Utc::now());

        assert_eq!(status_of(&store, &live), "pending");
        assert_eq!(status_of(&store, &no_session), "pending");
    }

    #[test]
    fn an_unattended_approval_inside_the_window_still_waits() {
        // The whole point of 72h is that a Friday-night run survives until
        // Monday. 71 hours in, the answer is still wanted.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(71));

        sweep_once(&store, Utc::now());

        assert_eq!(status_of(&store, &id), "pending");
    }

    #[test]
    fn expiry_never_touches_an_already_decided_row() {
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, &ago(500));
        store.resolve(&id, "approved", "ops@example.com").unwrap();

        sweep_once(&store, Utc::now());

        assert_eq!(
            status_of(&store, &id),
            "approved",
            "a decision that was actually made must outlive any sweep"
        );
    }

    #[test]
    fn an_unparseable_timestamp_is_left_alone() {
        // Retiring is the irreversible move here; doing it off a timestamp
        // we cannot read would be guessing.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let id = unattended(&store, "not-a-timestamp");

        sweep_once(&store, Utc::now());

        assert_eq!(status_of(&store, &id), "pending");
    }

    #[test]
    fn an_expired_row_never_reaches_the_redelivery_sweep() {
        // This is what makes "expiry is not denial" real rather than a
        // naming choice: `list_undelivered_resolved` is what buys a
        // continuation turn, and an expiry has nobody to deliver to.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let lapsed = unattended(&store, &ago(500));
        let denied = unattended(&store, &ago(500));
        store.resolve(&denied, "denied", "ops@example.com").unwrap();

        sweep_once(&store, Utc::now());

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

    #[test]
    fn an_expired_row_drops_out_of_the_pending_list() {
        // The Notification Centre asks for `status=pending`; this is the
        // whole user-visible effect of the sweep.
        let store = PendingApprovalsStore::new_in_memory().unwrap();
        let _old = unattended(&store, &ago(500));
        let live = insert(&store, Some("sess-42"), &ago(500));

        sweep_once(&store, Utc::now());

        let pending: Vec<String> = store
            .list(Some("pending"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(pending, vec![live]);
    }
}
