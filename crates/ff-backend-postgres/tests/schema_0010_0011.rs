//! RFC-020 Wave 9 schema smoke test — migrations 0010 + 0011.
//!
//! Applies every migration up through 0011 (sqlx is idempotent) and
//! asserts:
//!
//!   0010 `ff_operator_event` —
//!     * the outbox table exists and is not partitioned,
//!     * `execution_id` is `TEXT`,
//!     * CHECK constraint restricts `event_type` to the 3 operator-
//!       control events,
//!     * the three indexes defined in the migration exist,
//!     * the `pg_notify` trigger is installed,
//!     * `ff_migration_annotation` records `(10, _, _, true)`.
//!
//!   0011 `ff_waitpoint_pending` —
//!     * the 3 new columns exist with the right types + defaults,
//!     * `waitpoint_key` (from migration 0004) is still present as
//!       `TEXT NOT NULL DEFAULT ''` (RFC-020 Revision 5 ground-truth
//!       assertion),
//!     * `ff_migration_annotation` records `(11, _, _, true)`.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave9_test \
//!   cargo test -p ff-backend-postgres --test schema_0010_0011 -- --ignored
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

// ---------- 0010 `ff_operator_event` ----------

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_table_exists() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_class c \
         JOIN pg_namespace n ON c.relnamespace = n.oid \
         WHERE n.nspname = 'public' AND c.relname = 'ff_operator_event'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "ff_operator_event table must exist");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_is_not_partitioned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    // Partitioned tables have a row in pg_partitioned_table.
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_partitioned_table pt \
         JOIN pg_class c ON c.oid = pt.partrelid \
         WHERE c.relname = 'ff_operator_event'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 0, "ff_operator_event must NOT be partitioned");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_execution_id_is_text() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'ff_operator_event' \
           AND column_name = 'execution_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let dt: String = row.get("data_type");
    assert_eq!(dt, "text", "execution_id must be TEXT (not bytea)");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_type_check_constraint() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    // Valid values should insert.
    for et in &["priority_changed", "replayed", "flow_cancel_requested"] {
        sqlx::query(
            "INSERT INTO ff_operator_event \
               (execution_id, event_type, occurred_at_ms, partition_key) \
             VALUES ($1, $2, 0, 0)",
        )
        .bind("exec-test")
        .bind(et)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("valid event_type {et} should insert: {e}"));
    }
    // Invalid value should fail the CHECK.
    let res = sqlx::query(
        "INSERT INTO ff_operator_event \
           (execution_id, event_type, occurred_at_ms, partition_key) \
         VALUES ('exec-bad', 'not_an_operator_event', 0, 0)",
    )
    .execute(&pool)
    .await;
    assert!(
        res.is_err(),
        "CHECK constraint must reject unknown event_type"
    );
    // Clean up.
    sqlx::query("DELETE FROM ff_operator_event WHERE execution_id = 'exec-test'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_indexes_exist() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    for idx in &[
        "ff_operator_event_partition_key_idx",
        "ff_operator_event_namespace_idx",
        "ff_operator_event_instance_tag_idx",
    ] {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
             WHERE schemaname = 'public' AND indexname = $1",
        )
        .bind(idx)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 1, "index {idx} must exist");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_notify_trigger_exists() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         WHERE c.relname = 'ff_operator_event' \
           AND t.tgname = 'ff_operator_event_notify_trg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "NOTIFY trigger must be installed");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn operator_event_migration_annotation_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 10",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 10);
    assert_eq!(n, "0010_operator_event_outbox");
    assert!(bc, "0010 must be backward_compatible=true (additive)");
}

// ---------- 0011 `ff_waitpoint_pending` additive columns ----------

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn waitpoint_pending_new_columns_exist() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    // (column_name, data_type, is_nullable, column_default presence)
    let expected: &[(&str, &str, &str, bool)] = &[
        ("state", "text", "NO", true),
        ("required_signal_names", "ARRAY", "NO", true),
        ("activated_at_ms", "bigint", "YES", false),
    ];
    for (col, dt, nullable, has_default) in expected {
        let row = sqlx::query(
            "SELECT data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'ff_waitpoint_pending' \
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
async fn waitpoint_key_from_0004_still_present() {
    // RFC-020 Revision 5 ground-truth: `waitpoint_key` exists from
    // migration 0004 as TEXT NOT NULL DEFAULT ''. 0011 does NOT add
    // it. This test pins that invariant.
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'ff_waitpoint_pending' \
           AND column_name = 'waitpoint_key'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let dt: String = row.get("data_type");
    let nullable: String = row.get("is_nullable");
    let default: Option<String> = row.get("column_default");
    assert_eq!(dt, "text");
    assert_eq!(nullable, "NO", "waitpoint_key is NOT NULL since 0004");
    assert!(
        default
            .as_deref()
            .map(|d| d.contains("''"))
            .unwrap_or(false),
        "waitpoint_key must default to empty string (from 0004), got: {default:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn waitpoint_pending_migration_annotation_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 11",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 11);
    assert_eq!(n, "0011_waitpoint_pending_extensions");
    assert!(bc, "0011 must be backward_compatible=true (additive)");
}
