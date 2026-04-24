//! Wave 3 schema-0001 smoke test.
//!
//! Applies `migrations/0001_initial.sql` to a live Postgres and
//! asserts the genesis-schema invariants spelled out in
//! `rfcs/drafts/v0.7-migration-master.md`:
//!
//! * every partitioned table has exactly 256 child partitions
//!   (Q5 — `PARTITION BY HASH (partition_key) PARTITIONS 256`),
//! * the four global tables are NOT partitioned (Q4, Q5, Q6, Q12),
//! * `ff_partition_config` seeds `num_flow_partitions = 256`,
//! * `ff_migration_annotation` records the genesis row as
//!   `(1, '0001_initial', _, false)` (Q12),
//! * every named index from §2 of the Wave-3 brief exists.
//!
//! # Running
//!
//! The test is `#[ignore]` by default because it requires a live
//! Postgres. Set `FF_PG_TEST_URL` to a connection string pointing at
//! a *throwaway* database — the test drops/creates schema objects
//! inside it — and run:
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave3_test \
//!   cargo test -p ff-backend-postgres --test schema_0001 -- --ignored
//! ```
//!
//! CI wiring + a testcontainers harness land in a follow-up PR
//! once Wave 4 needs the live backend for proc testing.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// Resolve the test DB URL and apply `0001_initial.sql` if it hasn't
/// been applied yet (sqlx's migrate table is idempotent — reapplies
/// are no-ops). The test DB is expected to start empty; callers
/// running the suite multiple times will see the first run seed
/// schema, and subsequent runs re-use it.
///
/// Doing it this way (instead of `DROP SCHEMA CASCADE` between
/// tests) sidesteps pg's lock-slot budget — dropping 3 328 child
/// partitions in one tx exceeds `max_locks_per_transaction ×
/// max_connections` on stock installs, the same reason the
/// migration opts into `-- no-transaction`.
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

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn partition_children_are_256() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    for parent in [
        "ff_exec_core",
        "ff_flow_core",
        "ff_attempt",
        "ff_edge",
        "ff_edge_group",
        "ff_pending_cancel_groups",
        "ff_cancel_dispatch_outbox",
        "ff_suspension_current",
        "ff_waitpoint_pending",
        "ff_stream_frame",
        "ff_stream_summary",
        "ff_stream_meta",
        "ff_completion_event",
    ] {
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
async fn global_tables_are_not_partitioned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    for global in [
        "ff_waitpoint_hmac",
        "ff_lane_registry",
        "ff_partition_config",
        "ff_migration_annotation",
    ] {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_partitioned_table pt \
             JOIN pg_class c ON pt.partrelid = c.oid WHERE c.relname = $1",
        )
        .bind(global)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 0, "{global} must NOT be partitioned");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn partition_config_seed() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let row = sqlx::query(
        "SELECT num_flow_partitions, num_budget_partitions, num_quota_partitions, ff_version \
         FROM ff_partition_config",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let flow: i32 = row.get("num_flow_partitions");
    let budget: i32 = row.get("num_budget_partitions");
    let quota: i32 = row.get("num_quota_partitions");
    let ver: String = row.get("ff_version");
    assert_eq!(flow, 256);
    assert_eq!(budget, 256);
    assert_eq!(quota, 256);
    assert_eq!(ver, "0.7.0");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn migration_annotation_genesis_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let row = sqlx::query(
        "SELECT version, name, backward_compatible FROM ff_migration_annotation WHERE version = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let v: i32 = row.get("version");
    let n: String = row.get("name");
    let bc: bool = row.get("backward_compatible");
    assert_eq!(v, 1);
    assert_eq!(n, "0001_initial");
    assert!(!bc, "0001 must be backward_compatible=false (genesis)");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn named_indices_exist() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    // Indices named in the Wave-3 brief §2 + the additional ones
    // documented in the migration file. These are the parent-level
    // names; pg mirrors them onto each child partition with a
    // different name, but the parent name exists in pg_indexes.
    for idx in [
        "ff_exec_core_lane_phase_idx",
        "ff_exec_core_caps_gin_idx",
        "ff_exec_core_flow_idx",
        "ff_exec_core_deadline_idx",
        "ff_attempt_lease_expiry_idx",
        "ff_attempt_worker_idx",
        "ff_edge_downstream_idx",
        "ff_edge_upstream_idx",
        "ff_pending_cancel_groups_enqueued_idx",
        "ff_cancel_dispatch_outbox_enqueued_idx",
        "ff_suspension_current_suspended_at_idx",
        "ff_suspension_current_timeout_idx",
        "ff_waitpoint_pending_expires_idx",
        "ff_waitpoint_pending_exec_idx",
        "ff_stream_frame_ts_seq_idx",
        "ff_completion_event_event_id_idx",
    ] {
        let row = sqlx::query(
            "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
             WHERE schemaname='public' AND indexname = $1",
        )
        .bind(idx)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = row.get("n");
        assert_eq!(n, 1, "index {idx} must exist on its parent table");
    }
}
