//! RFC-020 Wave 9 Standalone-1 schema smoke test — migrations
//! 0012 (`ff_quota_policy` + window + admitted) + 0013 (budget
//! policy extensions).
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave9_s1_test \
//!   cargo test -p ff-backend-postgres --test schema_0012_0013 -- --ignored
//! ```

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

// ---------- 0012 `ff_quota_policy` + `ff_quota_window` + `ff_quota_admitted` ----------

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn quota_tables_exist_and_are_hash_partitioned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    for tbl in &["ff_quota_policy", "ff_quota_window", "ff_quota_admitted"] {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_partitioned_table pt \
             JOIN pg_class c ON c.oid = pt.partrelid \
             WHERE c.relname = $1 AND pt.partstrat = 'h'",
        )
        .bind(tbl)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 1, "{tbl} must be HASH-partitioned");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn quota_tables_have_256_partition_children() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    for tbl in &["ff_quota_policy", "ff_quota_window", "ff_quota_admitted"] {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n \
             FROM pg_inherits i \
             JOIN pg_class p ON p.oid = i.inhparent \
             WHERE p.relname = $1",
        )
        .bind(tbl)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 256, "{tbl} must have 256 partition children");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn quota_policy_has_active_concurrency_column() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT data_type, column_default FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'ff_quota_policy' \
           AND column_name = 'active_concurrency'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let dt: String = row.get("data_type");
    assert_eq!(dt, "bigint");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn quota_admitted_expires_index_exists() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
         WHERE schemaname = 'public' AND indexname = 'ff_quota_admitted_expires_idx'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "ff_quota_admitted_expires_idx must exist");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn quota_window_has_no_redundant_sweep_index() {
    // Round-6 reviewer finding: the PK covers sweep scans as a B-tree
    // prefix match; no separate `ff_quota_window_sweep_idx`.
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
         WHERE schemaname = 'public' AND indexname = 'ff_quota_window_sweep_idx'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 0, "redundant sweep index must not be created");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn migration_0012_annotation_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 12",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 12);
    assert_eq!(n, "0012_quota_policy");
    assert!(bc, "0012 must be backward_compatible=true (additive)");
}

// ---------- 0013 `ff_budget_policy` extensions ----------

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_policy_new_columns_exist() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    // (column_name, data_type, is_nullable, has_default)
    let expected: &[(&str, &str, &str, bool)] = &[
        ("scope_type", "text", "NO", true),
        ("scope_id", "text", "NO", true),
        ("enforcement_mode", "text", "NO", true),
        ("breach_count", "bigint", "NO", true),
        ("soft_breach_count", "bigint", "NO", true),
        ("last_breach_at_ms", "bigint", "YES", false),
        ("last_breach_dim", "text", "YES", false),
        ("next_reset_at_ms", "bigint", "YES", false),
    ];
    for (col, dt, nullable, has_default) in expected {
        let row = sqlx::query(
            "SELECT data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'ff_budget_policy' \
               AND column_name = $1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("column {col} must exist: {e}"));
        let got_dt: String = row.get("data_type");
        let got_null: String = row.get("is_nullable");
        let got_default: Option<String> = row.get("column_default");
        assert_eq!(&got_dt, dt, "column {col} data_type");
        assert_eq!(&got_null, nullable, "column {col} nullability");
        assert_eq!(
            got_default.is_some(),
            *has_default,
            "column {col} default presence"
        );
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_policy_reset_due_partial_index_exists() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = 'public' AND indexname = 'ff_budget_policy_reset_due_idx'",
    )
    .fetch_one(&pool)
    .await
    .expect("ff_budget_policy_reset_due_idx must exist");
    let def: String = row.get("indexdef");
    assert!(
        def.contains("next_reset_at_ms IS NOT NULL"),
        "partial index predicate must gate on NULL: {def}",
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn migration_0013_annotation_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 13",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 13);
    assert_eq!(n, "0013_budget_policy_extensions");
    assert!(bc, "0013 must be backward_compatible=true (additive)");
}
