//! Wave 3b schema-0002 smoke test.
//!
//! Applies `migrations/0002_budget.sql` (on top of `0001_initial.sql`)
//! and asserts the budget-schema invariants:
//!
//! * the three new parent tables exist,
//! * each parent has exactly 256 child partitions (Q5),
//! * `ff_migration_annotation` records the row
//!   `(2, '0002_budget', _, true)` (Q12),
//! * the PK constraints are present on every parent.
//!
//! Runs under the same `FF_PG_TEST_URL` harness as `schema_0001`;
//! see that file for rationale.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave3b_test \
//!   cargo test -p ff-backend-postgres --test schema_0002 -- --ignored
//! ```
//!
//! Note: `apply_migrations` is idempotent (sqlx tracks applied
//! versions), so this test implicitly re-exercises 0001 as well.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

const BUDGET_PARENTS: &[&str] = &[
    "ff_budget_policy",
    "ff_budget_usage",
    "ff_budget_usage_dedup",
];

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_parent_tables_exist() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    for parent in BUDGET_PARENTS {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_class c \
             JOIN pg_namespace n ON c.relnamespace = n.oid \
             WHERE n.nspname = 'public' AND c.relname = $1",
        )
        .bind(parent)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 1, "parent table {parent} must exist");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_partition_children_are_256() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    for parent in BUDGET_PARENTS {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_inherits \
             WHERE inhparent = ($1 || '')::regclass",
        )
        .bind(parent)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 256, "{parent} must have exactly 256 child partitions");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_migration_annotation_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 2",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 2);
    assert_eq!(n, "0002_budget");
    assert!(bc, "0002 must be backward_compatible=true (additive)");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_primary_keys_present() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    // Expected PK column lists, in order.
    let expected: &[(&str, &[&str])] = &[
        ("ff_budget_policy", &["partition_key", "budget_id"]),
        (
            "ff_budget_usage",
            &["partition_key", "budget_id", "dimensions_key"],
        ),
        (
            "ff_budget_usage_dedup",
            &["partition_key", "dedup_key"],
        ),
    ];

    for (table, cols) in expected {
        let row = sqlx::query(
            "SELECT a.attname AS col, array_position(i.indkey::int[], a.attnum::int) AS ord \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY(i.indkey) \
             WHERE c.relname = $1 AND i.indisprimary \
             ORDER BY ord",
        )
        .bind(table)
        .fetch_all(&pool)
        .await
        .unwrap();
        let got: Vec<String> = row.iter().map(|r| r.get::<String, _>("col")).collect();
        let expect: Vec<String> = cols.iter().map(|s| s.to_string()).collect();
        assert_eq!(got, expect, "PK columns mismatch on {table}");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_dedup_expires_index_exists() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
         WHERE schemaname='public' AND indexname = 'ff_budget_usage_dedup_expires_idx'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "ff_budget_usage_dedup_expires_idx must exist");
}
