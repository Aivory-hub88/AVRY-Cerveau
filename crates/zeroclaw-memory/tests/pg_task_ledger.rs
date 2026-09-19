//! Agent Task Ledger: integration tests for the Postgres-backed
//! `zeroclaw_memory::task_ledger`.
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set
//! (same convention as `pg_capability_graph.rs`) — a no-op otherwise so the
//! suite stays green without Postgres.
//!
//! Proven: create starts a task in `todo`; update_status moves it and
//! stores `blocked_reason`; a status filter on `list_tasks` only returns
//! matching rows; get_task fetches the same row by id, tenant-scoped; one
//! tenant can never update or fetch another tenant's task even by
//! guessing its id; a done task is never deleted (it just stops showing up
//! under a `todo`/`in_progress`/`blocked` filter).

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::task_ledger::{AgentTaskLedger, StatusUpdate, TaskPriority, TaskStatus};

const SCHEMA: &str = "cerveau_task_ledger_test";

fn pg_url() -> Option<String> {
    std::env::var("CERVEAU_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

async fn exec(sql: &str) {
    let url = pg_url().unwrap();
    let sql = sql.to_string();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        c.batch_execute(&sql).expect("admin exec");
    })
    .await
    .expect("admin task join");
}

#[tokio::test(flavor = "multi_thread")]
async fn task_ledger_end_to_end() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping task ledger test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;

    let ledger = AgentTaskLedger::connect(&url, SCHEMA)
        .await
        .expect("connect + init schema");

    // ── Scenario 1: create starts a task in todo ──
    let task_id = ledger
        .create_task(
            "tenant-a",
            "finance_invoice_ops",
            Some("session-1"),
            "Send Q3 invoice to Acme",
            TaskPriority::High,
        )
        .await
        .expect("create_task");

    let open = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", None)
        .await
        .expect("list_tasks");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].task_id, task_id);
    assert_eq!(open[0].status, TaskStatus::Todo);
    assert_eq!(open[0].priority, TaskPriority::High);
    assert_eq!(open[0].session_id.as_deref(), Some("session-1"));
    assert!(open[0].blocked_reason.is_none());

    // ── Scenario 2: update to blocked stores the reason ──
    ledger
        .update_status(
            "tenant-a",
            &task_id,
            TaskStatus::Blocked,
            Some("waiting on operator approval to send"),
        )
        .await
        .expect("update_status blocked");

    let blocked = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", Some(TaskStatus::Blocked))
        .await
        .expect("list_tasks blocked");
    assert_eq!(blocked.len(), 1);
    assert_eq!(
        blocked[0].blocked_reason.as_deref(),
        Some("waiting on operator approval to send")
    );

    let todo = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", Some(TaskStatus::Todo))
        .await
        .expect("list_tasks todo");
    assert!(
        todo.is_empty(),
        "task moved out of todo must not still show up under a todo filter"
    );

    // ── Scenario 2b: get_task fetches the same row by id, tenant-scoped ──
    let fetched = ledger
        .get_task("tenant-a", &task_id)
        .await
        .expect("get_task")
        .expect("task must exist");
    assert_eq!(fetched.status, TaskStatus::Blocked);
    assert_eq!(
        fetched.blocked_reason.as_deref(),
        Some("waiting on operator approval to send")
    );
    assert!(
        ledger
            .get_task("tenant-b", &task_id)
            .await
            .expect("get_task cross-tenant")
            .is_none(),
        "tenant-b must not be able to fetch tenant-a's task by id"
    );

    // ── Scenario 3: tenant isolation — another tenant can't touch this task ──
    let cross_tenant = ledger
        .update_status("tenant-b", &task_id, TaskStatus::Done, None)
        .await;
    assert!(
        cross_tenant.is_err(),
        "tenant-b must not be able to update tenant-a's task"
    );

    // ── Scenario 4: done is terminal, but the row is never deleted ──
    assert_eq!(
        ledger
            .update_status("tenant-a", &task_id, TaskStatus::Done, None)
            .await
            .expect("update_status done"),
        StatusUpdate::Applied
    );
    let all = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", None)
        .await
        .expect("list_tasks all");
    assert_eq!(all.len(), 1, "done tasks stay in the ledger, never deleted");
    assert_eq!(all[0].status, TaskStatus::Done);

    // ── Scenario 4b: a finished task explains itself, it is not "missing" ──
    // Regression: a second concurrent turn updating a task the first had just
    // finished got "task ... not found for this tenant" -- true of the live
    // table, wrong as a diagnosis, and it sent the agent hunting/retrying.
    let late = ledger
        .update_status("tenant-a", &task_id, TaskStatus::Blocked, Some("late"))
        .await
        .expect_err("changing a done task must be refused");
    let late = late.to_string();
    assert!(late.contains("already done"), "wrong diagnosis: {late}");
    assert!(late.contains("create a new task"), "not actionable: {late}");
    assert!(
        !late.contains("not found"),
        "must not read as missing: {late}"
    );
    // Finishing it again is idempotent (the goal state already holds) and must
    // not create a second archive row.
    assert_eq!(
        ledger
            .update_status("tenant-a", &task_id, TaskStatus::Done, None)
            .await
            .expect("done twice is idempotent"),
        StatusUpdate::AlreadyDone,
        "a repeated done must say nothing changed, not \"applied\""
    );
    assert_eq!(
        ledger
            .list_tasks("tenant-a", "finance_invoice_ops", None)
            .await
            .expect("list_tasks after double done")
            .len(),
        1,
        "double done must not duplicate the archived task"
    );
    // Another tenant must learn nothing about it: still plain "not found".
    for status in [TaskStatus::InProgress, TaskStatus::Done] {
        let err = ledger
            .update_status("tenant-b", &task_id, status, None)
            .await
            .expect_err("cross-tenant write to an archived task")
            .to_string();
        assert!(err.contains("not found"), "leaked existence: {err}");
        assert!(!err.contains("already done"), "leaked existence: {err}");
    }
    // A genuinely unknown id is still "not found".
    let unknown = ledger
        .update_status("tenant-a", "no-such-task", TaskStatus::InProgress, None)
        .await
        .expect_err("unknown task")
        .to_string();
    assert!(unknown.contains("not found"), "{unknown}");

    // ── Scenario 5: operator-cancelled rows resist late agent writes ──
    // `cancelled` is a dashboard-only terminal state (the Stop button writes
    // it directly — the tool schema has no Cancelled variant). A late
    // task_update_status from a still-running turn must be a silent no-op,
    // never a resurrection.
    let cancelled_id = ledger
        .create_task(
            "tenant-a",
            "finance_invoice_ops",
            Some("session-1"),
            "Stuck verification the operator stopped",
            TaskPriority::Normal,
        )
        .await
        .expect("create cancelled probe");
    exec(&format!(
        "UPDATE {SCHEMA}.agent_tasks SET status = 'cancelled', updated_at = NOW() \
         WHERE task_id = '{cancelled_id}';"
    ))
    .await;
    assert_eq!(
        ledger
            .update_status("tenant-a", &cancelled_id, TaskStatus::InProgress, None)
            .await
            .expect("late write to a cancelled row must succeed quietly, not error"),
        StatusUpdate::StoppedByOperator,
        "the caller must be able to tell the write was ignored"
    );
    let fetched = ledger
        .get_task("tenant-a", &cancelled_id)
        .await
        .expect("get_task cancelled")
        .expect("cancelled row must still exist");
    assert_eq!(
        fetched.status,
        TaskStatus::Cancelled,
        "a stopped task stays stopped even when the agent writes late"
    );

    // ── Scenario 5b: a late `done` must not turn "abandoned" into "delivered" ──
    // Regression: the archive DELETE had no status filter, so an agent's late
    // `done` on an operator-cancelled row archived it as Done, undoing the Stop.
    assert_eq!(
        ledger
            .update_status("tenant-a", &cancelled_id, TaskStatus::Done, None)
            .await
            .expect("late done on a cancelled row is a silent no-op"),
        StatusUpdate::StoppedByOperator,
        "a late done on a stopped task must be reported as ignored"
    );
    let after_done = ledger
        .get_task("tenant-a", &cancelled_id)
        .await
        .expect("get_task after late done")
        .expect("cancelled row must still exist");
    assert_eq!(
        after_done.status,
        TaskStatus::Cancelled,
        "a stopped task stays stopped even when the agent finishes it late"
    );
    // ...and it must not have leaked into the archive as a delivered task.
    let archived_dupes = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", Some(TaskStatus::Done))
        .await
        .expect("list done tasks")
        .into_iter()
        .filter(|t| t.task_id == cancelled_id)
        .count();
    assert_eq!(archived_dupes, 0, "cancelled task archived as done");
    // Another tenant still learns nothing about it.
    assert!(
        ledger
            .update_status("tenant-b", &cancelled_id, TaskStatus::Done, None)
            .await
            .expect_err("cross-tenant done on a cancelled row")
            .to_string()
            .contains("not found")
    );

    // ── Scenario 6: orphan sweep parks dead in_progress rows ──
    // A new turn in session-2 must park session-1's stale in_progress row
    // (older than the sweep threshold) but leave same-session and fresh
    // rows alone.
    let orphan_id = ledger
        .create_task(
            "tenant-a",
            "finance_invoice_ops",
            Some("session-1"),
            "Old turn that died",
            TaskPriority::Normal,
        )
        .await
        .expect("create orphan probe");
    let live_id = ledger
        .create_task(
            "tenant-a",
            "finance_invoice_ops",
            Some("session-2"),
            "Current turn work",
            TaskPriority::Normal,
        )
        .await
        .expect("create live probe");
    ledger
        .update_status("tenant-a", &orphan_id, TaskStatus::InProgress, None)
        .await
        .expect("orphan to in_progress");
    ledger
        .update_status("tenant-a", &live_id, TaskStatus::InProgress, None)
        .await
        .expect("live to in_progress");
    exec(&format!(
        "UPDATE {SCHEMA}.agent_tasks SET updated_at = NOW() - INTERVAL '2 hours' \
         WHERE task_id = '{orphan_id}';"
    ))
    .await;
    let parked = ledger
        .park_orphaned_tasks("tenant-a", "finance_invoice_ops", Some("session-2"))
        .await
        .expect("sweep");
    assert_eq!(parked, 1, "exactly the stale foreign-session row parks");
    let orphan = ledger
        .get_task("tenant-a", &orphan_id)
        .await
        .expect("get orphan")
        .expect("orphan row still exists");
    assert_eq!(orphan.status, TaskStatus::Blocked);
    assert!(
        orphan
            .blocked_reason
            .as_deref()
            .unwrap_or("")
            .contains("Orphaned"),
        "park reason must say orphaned: {:?}",
        orphan.blocked_reason
    );
    let live = ledger
        .get_task("tenant-a", &live_id)
        .await
        .expect("get live")
        .expect("live row still exists");
    assert_eq!(live.status, TaskStatus::InProgress);
    // No session scope → no sweep (cannot tell dead from alive).
    let noscope = ledger
        .park_orphaned_tasks("tenant-a", "finance_invoice_ops", None)
        .await
        .expect("unscoped sweep");
    assert_eq!(noscope, 0);

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;")).await;
}
