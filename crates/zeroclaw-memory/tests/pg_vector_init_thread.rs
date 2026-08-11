//! Regression test for the pgvector-init-thread crash (queued fork patch,
//! ADR-004 §4b item 1 / CERVEAU-STATUS.md §6).
//!
//! `PostgresMemory::new(..., pgvector_enabled: Some(true), ...)` used to call
//! `try_enable_pgvector` back on whatever thread invoked `new()`. That call
//! drives the sync `postgres::Client`'s own internal Tokio runtime via
//! `block_on`; when `new()` is invoked synchronously from within an async
//! task already running on a Tokio runtime (exactly how the daemon
//! constructs its memory backend), entering a second runtime from that
//! thread panicked ("Cannot start a runtime from within a runtime"),
//! crash-looping the daemon. The fix folds `try_enable_pgvector` into the
//! same dedicated OS thread already used for schema init/migration.
//!
//! This test calls `PostgresMemory::new` with `pgvector_enabled = true`
//! directly from inside a multi-threaded Tokio test body — the same shape
//! that used to panic — and asserts it does not.
//!
//! Requires a live Postgres with the `vector` extension available (CI's
//! pgvector service container). Absent `CERVEAU_TEST_PG_URL` the test is a
//! no-op.

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::postgres::PostgresMemory;

const SCHEMA: &str = "cerveau_vecinit";

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
async fn pgvector_enable_does_not_panic_on_a_tokio_worker_thread() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping pgvector-init-thread test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
    ))
    .await;

    // Called directly (not via spawn_blocking) so this runs on a live Tokio
    // worker thread — the exact reproduction shape of the daemon's own
    // construction path.
    let result = PostgresMemory::new(
        "test",
        &url,
        SCHEMA,
        "memories",
        Some(5),
        Some(true),
        Some(64),
        None,
        0.7,
        0.3,
    );

    assert!(
        result.is_ok(),
        "PostgresMemory::new with pgvector_enabled=true panicked or errored from a Tokio worker thread: {:?}",
        result.err()
    );

    exec(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;")).await;
}
