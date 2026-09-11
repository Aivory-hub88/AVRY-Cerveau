//! Agent Task Ledger: integration tests for the Postgres-backed
//! `zeroclaw_memory::task_ledger`.
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set
//! (same convention as `pg_capability_graph.rs`) — a no-op otherwise so the
//! suite stays green without Postgres.
//!
//! Proven: create starts a task in `todo`; update_status moves it and
//! stores `blocked_reason`; a status filter on `list_tasks` only returns
//! matching rows; one tenant can never update another tenant's task even
//! by guessing its id; a done task is never deleted (it just stops showing
//! up under a `todo`/`in_progress`/`blocked` filter).

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::task_ledger::{AgentTaskLedger, TaskPriority, TaskStatus};

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

    // ── Scenario 3: tenant isolation — another tenant can't touch this task ──
    let cross_tenant = ledger
        .update_status("tenant-b", &task_id, TaskStatus::Done, None)
        .await;
    assert!(
        cross_tenant.is_err(),
        "tenant-b must not be able to update tenant-a's task"
    );

    // ── Scenario 4: done is terminal, but the row is never deleted ──
    ledger
        .update_status("tenant-a", &task_id, TaskStatus::Done, None)
        .await
        .expect("update_status done");
    let all = ledger
        .list_tasks("tenant-a", "finance_invoice_ops", None)
        .await
        .expect("list_tasks all");
    assert_eq!(all.len(), 1, "done tasks stay in the ledger, never deleted");
    assert_eq!(all[0].status, TaskStatus::Done);

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;")).await;
}
