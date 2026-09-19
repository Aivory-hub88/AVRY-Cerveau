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

use zeroclaw_memory::task_ledger::{
    AdoptResult, AgentTaskLedger, DelegationEnd, NewDelegatedTask, StatusUpdate, TaskOutcome,
    TaskPriority, TaskStatus,
};

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

// ── ADR-014 A2: the delegation link ─────────────────────────────────────

const DELEGATION_SCHEMA: &str = "cerveau_task_ledger_delegation_test";

/// A live row created by the engine for a background delegation.
fn delegated<'a>(
    tenant: &'a str,
    agent_type: &'a str,
    delegation_id: &'a str,
) -> NewDelegatedTask<'a> {
    NewDelegatedTask {
        tenant_id: tenant,
        agent_type,
        session_id: Some("room-1"),
        title: "Delegated to leads_qualifier: qualify the new leads",
        delegated_by: "chief_of_staff",
        delegation_id,
        context_id: None,
        status: TaskStatus::InProgress,
        outcome: None,
        blocked_reason: None,
        result_summary: None,
    }
}

async fn age(schema: &str, task_id: &str) {
    exec(&format!(
        "UPDATE {schema}.agent_tasks SET updated_at = NOW() - INTERVAL '2 hours' \
         WHERE task_id = '{task_id}'"
    ))
    .await;
}

async fn set_status(schema: &str, task_id: &str, status: &str) {
    exec(&format!(
        "UPDATE {schema}.agent_tasks SET status = '{status}' WHERE task_id = '{task_id}'"
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delegated_rows_are_projections_of_a_delegation() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping delegation link test");
        return;
    };
    let schema = DELEGATION_SCHEMA;
    exec(&format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"
    ))
    .await;
    let ledger = AgentTaskLedger::connect(&url, schema)
        .await
        .expect("connect + init schema");

    // ── 1. a delegated row carries its link and is found by delegation id ──
    let id = ledger
        .create_delegated_task(delegated("tenant-a", "leads_qualifier", "dlg-1"))
        .await
        .expect("create_delegated_task");
    let row = ledger
        .get_task("tenant-a", &id)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(row.status, TaskStatus::InProgress);
    assert_eq!(row.agent_type, "leads_qualifier");
    assert_eq!(row.delegated_by.as_deref(), Some("chief_of_staff"));
    assert_eq!(row.delegation_id.as_deref(), Some("dlg-1"));
    assert_eq!(row.session_id.as_deref(), Some("room-1"));
    assert!(row.outcome.is_none() && row.result_summary.is_none());
    assert_eq!(
        ledger.delegation_status("dlg-1").await.unwrap(),
        Some(TaskStatus::InProgress)
    );
    assert_eq!(ledger.delegation_status("nope").await.unwrap(), None);
    assert_eq!(ledger.open_delegation_ids().await.unwrap(), vec!["dlg-1"]);

    // ── 2. the 30-minute orphan sweep never buries a delegated row ──
    let plain = ledger
        .create_task(
            "tenant-a",
            "leads_qualifier",
            Some("room-1"),
            "own task",
            TaskPriority::Normal,
        )
        .await
        .unwrap();
    set_status(schema, &plain, "in_progress").await;
    age(schema, &id).await;
    age(schema, &plain).await;
    let parked = ledger
        .park_orphaned_tasks("tenant-a", "leads_qualifier", Some("another-session"))
        .await
        .unwrap();
    assert_eq!(parked, 1, "only the agent's own stale row is parked");
    assert_eq!(
        ledger
            .get_task("tenant-a", &id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::InProgress,
        "a delegated row belongs to the delegation, not to the sweep"
    );

    // ── 3. completed: archived with the summary; a second finish is a no-op ──
    assert!(
        ledger
            .finish_delegation(
                "dlg-1",
                DelegationEnd::Completed {
                    summary: Some("qualified 3 leads".into()),
                },
            )
            .await
            .unwrap()
    );
    assert!(
        ledger.get_task("tenant-a", &id).await.unwrap().is_none(),
        "left the live table"
    );
    let done = ledger
        .list_tasks("tenant-a", "leads_qualifier", Some(TaskStatus::Done))
        .await
        .unwrap();
    let archived = done.iter().find(|t| t.task_id == id).expect("archived");
    assert_eq!(
        archived.result_summary.as_deref(),
        Some("qualified 3 leads")
    );
    assert_eq!(archived.delegation_id.as_deref(), Some("dlg-1"));
    assert_eq!(archived.delegated_by.as_deref(), Some("chief_of_staff"));
    assert!(
        !ledger
            .finish_delegation("dlg-1", DelegationEnd::Completed { summary: None })
            .await
            .unwrap(),
        "idempotent: nothing left to finish"
    );

    // ── 4. failure is `blocked` + an outcome, never a new status ──
    ledger
        .create_delegated_task(delegated("tenant-a", "leads_qualifier", "dlg-2"))
        .await
        .unwrap();
    assert!(
        ledger
            .finish_delegation(
                "dlg-2",
                DelegationEnd::Failed {
                    outcome: TaskOutcome::TimedOut,
                    reason: "Delegation timed out: Agent 'lex' timed out after 300s".into(),
                },
            )
            .await
            .unwrap()
    );
    let failed = ledger
        .list_tasks("tenant-a", "leads_qualifier", Some(TaskStatus::Blocked))
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.delegation_id.as_deref() == Some("dlg-2"))
        .expect("failed delegation is on the board as blocked");
    assert_eq!(failed.outcome, Some(TaskOutcome::TimedOut));
    assert!(
        failed
            .blocked_reason
            .unwrap()
            .starts_with("Delegation timed out")
    );
    assert!(
        !ledger
            .open_delegation_ids()
            .await
            .unwrap()
            .contains(&"dlg-2".to_string()),
        "a settled row is no longer open"
    );

    // ── 5. waiting on a person is `blocked` without an outcome ──
    ledger
        .create_delegated_task(delegated("tenant-a", "leads_qualifier", "dlg-3"))
        .await
        .unwrap();
    assert!(
        ledger
            .finish_delegation(
                "dlg-3",
                DelegationEnd::InputRequired {
                    reason: "Waiting for approval: send_email (pa_9)".into(),
                },
            )
            .await
            .unwrap()
    );
    let waiting = ledger
        .list_tasks("tenant-a", "leads_qualifier", Some(TaskStatus::Blocked))
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.delegation_id.as_deref() == Some("dlg-3"))
        .unwrap();
    assert!(waiting.outcome.is_none());
    assert!(waiting.blocked_reason.unwrap().contains("send_email"));

    // ── 6. the operator's Stop is seen, and never overwritten ──
    let stop_id = ledger
        .create_delegated_task(delegated("tenant-a", "leads_qualifier", "dlg-4"))
        .await
        .unwrap();
    set_status(schema, &stop_id, "cancelled").await; // what the dashboard does
    assert_eq!(
        ledger.delegation_status("dlg-4").await.unwrap(),
        Some(TaskStatus::Cancelled)
    );
    for end in [
        DelegationEnd::Completed {
            summary: Some("late".into()),
        },
        DelegationEnd::Failed {
            outcome: TaskOutcome::Failed,
            reason: "late".into(),
        },
        DelegationEnd::InputRequired {
            reason: "late".into(),
        },
        DelegationEnd::Cancelled {
            reason: "late".into(),
        },
    ] {
        assert!(
            !ledger.finish_delegation("dlg-4", end).await.unwrap(),
            "a late write must never touch an operator-stopped row"
        );
    }
    assert_eq!(
        ledger
            .get_task("tenant-a", &stop_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled
    );

    // ── 7. an engine-side cancel is recorded ──
    let cancel_id = ledger
        .create_delegated_task(delegated("tenant-a", "leads_qualifier", "dlg-5"))
        .await
        .unwrap();
    assert!(
        ledger
            .finish_delegation(
                "dlg-5",
                DelegationEnd::Cancelled {
                    reason: "Cancelled by caller".into()
                }
            )
            .await
            .unwrap()
    );
    assert_eq!(
        ledger
            .get_task("tenant-a", &cancel_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled
    );

    // ── 8. adopting a row the caller already created ──
    let mine = ledger
        .create_task(
            "tenant-a",
            "chief_of_staff",
            Some("room-1"),
            "Qualify new leads",
            TaskPriority::High,
        )
        .await
        .unwrap();
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-a", &mine, "dlg-6", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::Adopted
    );
    let adopted = ledger.get_task("tenant-a", &mine).await.unwrap().unwrap();
    assert_eq!(adopted.status, TaskStatus::InProgress);
    assert_eq!(adopted.delegation_id.as_deref(), Some("dlg-6"));
    assert_eq!(
        adopted.agent_type, "chief_of_staff",
        "adopting keeps the row's own owner"
    );
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-a", &mine, "dlg-7", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::AlreadyDelegated,
        "one row cannot track two delegations"
    );
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-b", &mine, "dlg-8", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::NotFound,
        "another tenant's id reads as not found"
    );
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-a", &stop_id, "dlg-9", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::Cancelled
    );
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-a", &id, "dlg-10", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::Done,
        "`done` is terminal"
    );
    assert_eq!(
        ledger
            .adopt_for_delegation("tenant-a", "no-such-task", "dlg-11", "chief_of_staff", None)
            .await
            .unwrap(),
        AdoptResult::NotFound
    );

    // ── 9. tenants never see each other's delegated rows ──
    ledger
        .create_delegated_task(delegated("tenant-b", "leads_qualifier", "dlg-b"))
        .await
        .unwrap();
    let a_rows = ledger
        .list_tasks("tenant-a", "leads_qualifier", None)
        .await
        .unwrap();
    assert!(a_rows.iter().all(|t| t.tenant_id == "tenant-a"));

    // ── 10. a sync hop that failed is recorded after the fact ──
    let mut lazy = delegated("tenant-a", "leads_qualifier", "dlg-sync");
    lazy.status = TaskStatus::Blocked;
    lazy.outcome = Some(TaskOutcome::Failed);
    lazy.blocked_reason = Some("Delegation failed: provider unreachable");
    let lazy_id = ledger.create_delegated_task(lazy).await.unwrap();
    let lazy_row = ledger
        .get_task("tenant-a", &lazy_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lazy_row.status, TaskStatus::Blocked);
    assert_eq!(lazy_row.outcome, Some(TaskOutcome::Failed));
    assert!(
        !ledger
            .open_delegation_ids()
            .await
            .unwrap()
            .contains(&"dlg-sync".to_string()),
        "a blocked row is settled, the reconciler must not touch it"
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}

/// The production table already exists with the pre-A2 ten columns. Connecting
/// must upgrade it in place, keep its rows readable, and be repeatable.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_a2_table_upgrades_in_place() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping legacy upgrade test");
        return;
    };
    let schema = "cerveau_task_ledger_upgrade_test";
    exec(&format!(
        r#"DROP SCHEMA IF EXISTS {schema} CASCADE;
           CREATE SCHEMA {schema};
           CREATE TABLE {schema}.agent_tasks (
               task_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, agent_type TEXT NOT NULL,
               session_id TEXT, title TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'todo',
               priority TEXT NOT NULL DEFAULT 'normal', blocked_reason TEXT,
               created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
               updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
           CREATE TABLE {schema}.agent_tasks_archive (
               task_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, agent_type TEXT NOT NULL,
               archived_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), payload BYTEA NOT NULL);
           INSERT INTO {schema}.agent_tasks (task_id, tenant_id, agent_type, title, status)
               VALUES ('old-1', 'tenant-a', 'lex', 'written before A2', 'in_progress');"#
    ))
    .await;

    let ledger = AgentTaskLedger::connect(&url, schema)
        .await
        .expect("upgrade on connect");
    let old = ledger
        .get_task("tenant-a", "old-1")
        .await
        .unwrap()
        .expect("old row readable");
    assert_eq!(old.title, "written before A2");
    assert_eq!(old.status, TaskStatus::InProgress);
    assert!(old.delegation_id.is_none() && old.outcome.is_none() && old.context_id.is_none());

    // Connecting again (every daemon start) is a no-op.
    drop(ledger);
    let ledger = AgentTaskLedger::connect(&url, schema)
        .await
        .expect("second connect");

    // An archive payload written before A2 has none of the new fields.
    let legacy = serde_json::json!({
        "task_id": "old-2", "tenant_id": "tenant-a", "agent_type": "lex",
        "session_id": null, "title": "archived before A2", "status": "done",
        "priority": "normal", "blocked_reason": null,
        "created_at": "2026-09-01T00:00:00Z", "updated_at": "2026-09-01T00:00:00Z"
    });
    let payload = {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&serde_json::to_vec(&legacy).unwrap())
            .unwrap();
        enc.finish().unwrap()
    };
    let url2 = url.clone();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url2, postgres::NoTls).unwrap();
        c.execute(
            &format!(
                "INSERT INTO {schema}.agent_tasks_archive (task_id, tenant_id, agent_type, payload) \
                 VALUES ($1, $2, $3, $4)"
            ),
            &[&"old-2", &"tenant-a", &"lex", &payload],
        )
        .unwrap();
    })
    .await
    .unwrap();
    let archived = ledger
        .list_tasks("tenant-a", "lex", Some(TaskStatus::Done))
        .await
        .unwrap();
    let row = archived
        .iter()
        .find(|t| t.task_id == "old-2")
        .expect("legacy archive row parses");
    assert_eq!(row.title, "archived before A2");
    assert!(row.delegation_id.is_none() && row.result_summary.is_none());

    // A row an agent finishes the ordinary way still archives (the shared
    // archive helper now also carries the delegation columns).
    ledger
        .update_status("tenant-a", "old-1", TaskStatus::Done, None)
        .await
        .expect("ordinary done still archives");
    assert!(
        ledger
            .get_task("tenant-a", "old-1")
            .await
            .unwrap()
            .is_none()
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}

// ── Connection loss: the ledger heals itself ─────────────────────────────

/// Terminate every other backend of the test database — what a Postgres restart,
/// a failover or a firewall killing an idle connection looks like to the ledger.
async fn kill_other_connections() {
    let url = pg_url().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        c.execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .expect("terminate backends");
    })
    .await
    .expect("admin join");
}

#[tokio::test(flavor = "multi_thread")]
async fn ledger_reconnects_after_its_connection_is_killed() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping ledger reconnect test");
        return;
    };
    let schema = "cerveau_task_ledger_reconnect_test";
    exec(&format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"
    ))
    .await;
    let ledger = AgentTaskLedger::connect(&url, schema)
        .await
        .expect("connect");
    let id = ledger
        .create_task("t", "a", Some("s"), "before the drop", TaskPriority::Normal)
        .await
        .expect("works before the drop");

    // A real outage: the ledger is idle when its connection dies. Wait past the idle
    // check, then use every kind of operation: none may stay broken.
    kill_other_connections().await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let listed = ledger
        .list_tasks("t", "a", None)
        .await
        .expect("a read heals the connection and succeeds");
    assert_eq!(listed.len(), 1, "no data was lost: {listed:?}");
    ledger
        .update_status("t", &id, TaskStatus::InProgress, None)
        .await
        .expect("a write works on the healed connection");
    ledger
        .create_task("t", "a", Some("s"), "after the drop", TaskPriority::Normal)
        .await
        .expect("create works after the drop");
    assert_eq!(ledger.list_tasks("t", "a", None).await.unwrap().len(), 2);

    // A drop that lands right before a call (the client has not noticed yet) may fail
    // that ONE call — it must not fail the ones after it.
    kill_other_connections().await;
    let _maybe_failed = ledger.list_tasks("t", "a", None).await;
    for attempt in 1..=3 {
        if ledger.list_tasks("t", "a", None).await.is_ok() {
            break;
        }
        assert!(attempt < 3, "still broken after {attempt} attempts");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // The archive path (a transaction) and the delegation path heal too.
    kill_other_connections().await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    ledger
        .update_status("t", &id, TaskStatus::Done, None)
        .await
        .expect("a transaction (archive on done) works after a drop");
    assert!(
        ledger
            .list_tasks("t", "a", Some(TaskStatus::Done))
            .await
            .unwrap()
            .iter()
            .any(|t| t.task_id == id)
    );

    exec(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;")).await;
}
