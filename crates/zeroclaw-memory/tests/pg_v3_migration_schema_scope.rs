//! Regression test for the upstream v3-memory-migration schema-scope bug
//! (ADR-004 §5).
//!
//! `migrate_postgres_memory_to_v3` checks constraint existence via
//! `pg_constraint.conname` alone, which is not unique across schemas. Before
//! the fix, migrating a second schema whose `memories` table reused the same
//! constraint names would see the first schema's constraint, skip its own
//! `ADD ... NOT VALID`, then fail at `VALIDATE CONSTRAINT` because the
//! constraint does not exist in *its* schema. This reproduces deterministically
//! for any two schemas, not just concurrent ones.
//!
//! Requires a live Postgres; runs only when `CERVEAU_TEST_PG_URL` is set.

#![cfg(feature = "memory-postgres")]

use zeroclaw_memory::postgres::PostgresMemory;

const SCHEMA_A: &str = "cerveau_v3scope_a";
const SCHEMA_B: &str = "cerveau_v3scope_b";

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

async fn constraint_exists_in_schema(schema: &str, table: &str, conname: &str) -> bool {
    let url = pg_url().unwrap();
    let schema = schema.to_string();
    let table = table.to_string();
    let conname = conname.to_string();
    tokio::task::spawn_blocking(move || {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        let row = c
            .query_one(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_constraint c
                    JOIN pg_class t ON t.oid = c.conrelid
                    JOIN pg_namespace n ON n.oid = t.relnamespace
                    WHERE c.conname = $1 AND n.nspname = $2 AND t.relname = $3
                )",
                &[&conname, &schema, &table],
            )
            .expect("query");
        row.get::<_, bool>(0)
    })
    .await
    .expect("task join")
}

/// Two schemas, both with a `memories` table (same name, same constraint
/// names), migrated to v3 one after the other. Before the fix this failed
/// on the second schema's `VALIDATE CONSTRAINT` step because the unscoped
/// existence check saw the first schema's constraint and skipped adding its
/// own.
#[tokio::test(flavor = "multi_thread")]
async fn v3_migration_is_schema_scoped() {
    let Some(url) = pg_url() else {
        eprintln!("CERVEAU_TEST_PG_URL unset — skipping v3 migration schema-scope test");
        return;
    };

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA_A} CASCADE; DROP SCHEMA IF EXISTS {SCHEMA_B} CASCADE;"
    ))
    .await;

    PostgresMemory::new("test_a", &url, SCHEMA_A, "memories", Some(5), Some(false), None)
        .expect("migrate schema A");
    PostgresMemory::new("test_b", &url, SCHEMA_B, "memories", Some(5), Some(false), None)
        .expect("migrate schema B — this failed pre-fix");

    for schema in [SCHEMA_A, SCHEMA_B] {
        for conname in [
            "memories_agent_id_notnull_chk",
            "memories_agent_id_fk",
            "memories_agent_key_uniq",
        ] {
            assert!(
                constraint_exists_in_schema(schema, "memories", conname).await,
                "expected {conname} to exist in schema {schema}"
            );
        }
    }

    exec(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA_A} CASCADE; DROP SCHEMA IF EXISTS {SCHEMA_B} CASCADE;"
    ))
    .await;
}
