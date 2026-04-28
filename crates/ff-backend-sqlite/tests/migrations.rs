//! RFC-023 Phase 1b — migration applier regression tests.
//!
//! Phase 1a shipped an empty migrations dir; Phase 1b lands 14
//! hand-ported SQLite-dialect migrations and wires `sqlx::migrate!`
//! into `SqliteBackend::new`. These tests assert the constructor
//! applies every migration against a fresh in-memory DB and that
//! the schema-of-record matches the ports.

use ff_backend_sqlite::SqliteBackend;
use serial_test::serial;

/// Construct a `SqliteBackend` against a fresh shared-cache `:memory:`
/// DB under FF_DEV_MODE=1. The helper embeds a UUID in the URI so
/// parallel tests never collide on the registry key.
async fn fresh_backend() -> std::sync::Arc<SqliteBackend> {
    // SAFETY: test-only env mutation; every caller is tagged
    // `#[serial(ff_dev_mode)]` which serialises all FF_DEV_MODE
    // readers + writers across this crate's test binaries, so no
    // concurrent reader observes the write (the UB vector `set_var`
    // is marked unsafe for in Rust 1.87+).
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-migrations-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri)
        .await
        .expect("construct with migrations")
}

/// Cheap unique-id helper; avoids pulling a new `uuid` dev-dep.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Thread-id disambiguates parallel `#[tokio::test]` runners.
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn migrations_apply_and_schema_is_present() {
    let backend = fresh_backend().await;
    let pool = backend.pool_for_test();

    // Spot-check a handful of table names drawn from every
    // contributing migration. `sqlite_master` is the canonical SQLite
    // catalog; `type='table'` filters out indexes and views.
    // `_sqlx_migrations` IS a table and is expected — asserted below.
    let mut rows =
        sqlx::query_as::<_, (String,)>("SELECT name FROM sqlite_master WHERE type='table'")
            .fetch_all(pool)
            .await
            .expect("query sqlite_master");
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<String> = rows.into_iter().map(|(n,)| n).collect();

    for expected in [
        // 0001 — genesis schema
        "ff_waitpoint_hmac",
        "ff_lane_registry",
        "ff_partition_config",
        "ff_migration_annotation",
        "ff_exec_core",
        "ff_execution_capabilities",
        "ff_flow_core",
        "ff_attempt",
        "ff_edge",
        "ff_edge_group",
        "ff_suspension_current",
        "ff_waitpoint_pending",
        "ff_stream_frame",
        "ff_completion_event",
        // 0002 — budget
        "ff_budget_policy",
        "ff_budget_usage",
        "ff_budget_usage_dedup",
        // 0004 — suspend-signal
        "ff_suspend_dedup",
        // 0006 / 0007 — lease + signal event outbox
        "ff_lease_event",
        "ff_signal_event",
        // 0010 — operator event outbox
        "ff_operator_event",
        // 0012 — quota policy
        "ff_quota_policy",
        "ff_quota_window",
        "ff_quota_admitted",
        // 0014 — cancel backlog
        "ff_cancel_backlog",
        "ff_cancel_backlog_member",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table `{expected}` not present; got {names:?}"
        );
    }

    // sqlx migration metadata table must exist post-apply.
    assert!(
        names.iter().any(|n| n == "_sqlx_migrations"),
        "sqlx migration metadata table missing; got {names:?}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn sqlx_migration_ledger_records_all_14() {
    // Naming kept for git-diff continuity; the real count floats as
    // new migrations land. Current set: 0001..=0014 + 0016 (RFC-020
    // Wave 9 follow-up #356; 0015 reserved for RFC-024 claim-grant).
    let backend = fresh_backend().await;
    let pool = backend.pool_for_test();

    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(pool)
            .await
            .expect("query _sqlx_migrations");
    assert_eq!(count, 15, "expected 15 successful migrations (0001..=0014 + 0016), got {count}");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn ff_execution_capabilities_junction_schema() {
    // RFC-023 §4.1 A4: normalized junction replaces the PG `text[] +
    // GIN` shape. Verify the junction table + reverse index.
    let backend = fresh_backend().await;
    let pool = backend.pool_for_test();

    // `pragma_table_info` is a table-valued function exposed by
    // SQLite >= 3.16; projects to (cid, name, type, notnull, dflt_value, pk).
    let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as("SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('ff_execution_capabilities')")
            .fetch_all(pool)
            .await
            .expect("pragma_table_info");
    let by_name: std::collections::HashMap<_, _> =
        cols.iter().map(|c| (c.1.as_str(), c)).collect();

    let exec = by_name
        .get("execution_id")
        .expect("execution_id column present");
    assert_eq!(exec.2, "BLOB");
    assert_eq!(exec.3, 1, "execution_id NOT NULL");
    assert_eq!(exec.5, 1, "execution_id part of PK");

    let cap = by_name
        .get("capability")
        .expect("capability column present");
    assert_eq!(cap.2, "TEXT");
    assert_eq!(cap.3, 1, "capability NOT NULL");
    assert_eq!(cap.5, 2, "capability second PK column");

    // Reverse index for capability-first routing must exist.
    let (idx_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type='index' AND name='ff_execution_capabilities_capability_idx'",
    )
    .fetch_one(pool)
    .await
    .expect("query reverse index");
    assert_eq!(idx_count, 1, "capability reverse index missing");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn migration_annotation_ledger_populated() {
    // Every migration INSERTs a row into `ff_migration_annotation`;
    // verify the expected set landed. 0015 is reserved for RFC-024
    // (claim-grant table) and not yet shipped, so the current ledger
    // is 0001..=0014 + 0016.
    let backend = fresh_backend().await;
    let pool = backend.pool_for_test();

    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT version FROM ff_migration_annotation ORDER BY version")
            .fetch_all(pool)
            .await
            .expect("query annotations");
    let versions: Vec<i64> = rows.into_iter().map(|(v,)| v).collect();
    let mut expected: Vec<i64> = (1..=14).collect();
    expected.push(16);
    assert_eq!(versions, expected);
}
