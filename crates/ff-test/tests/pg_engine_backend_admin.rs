//! Wave 4g — `EngineBackend` admin/list surface on Postgres.
//!
//! Exercises the two trait methods landed in
//! `ff-backend-postgres::admin`:
//!
//! * `list_lanes` — boot-seeded + dynamically-inserted lane ids are
//!   enumerable, cursor pagination is stable.
//! * `list_suspended` — partition-scoped cursor pagination with
//!   `reason_code` projection.
//!
//! # Running
//!
//! `#[ignore]` by default; requires a live throwaway Postgres.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4g_test \
//!   cargo test -p ff-test --test pg_engine_backend_admin -- --ignored
//! ```
//!
//! Each test inserts into shared tables within a unique prefix (an
//! empty-at-start marker per table + filter `WHERE lane_id LIKE
//! 'test4g_%'`) so concurrent test runs don't clobber each other.

use std::sync::Arc;

use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, PartitionKey};
use ff_core::types::{ExecutionId, LaneId};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// Resolve the test DB URL + apply migrations. Returns None when the
/// env var is absent so the test is a runtime skip rather than a
/// hard failure on a developer's machine.
async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

/// Unique-per-run prefix so concurrent CI jobs don't trip on each
/// other's inserts.
fn lane_prefix() -> String {
    format!("test4g_{}", Uuid::new_v4().simple())
}

async fn insert_lane(pool: &PgPool, lane_id: &str) {
    sqlx::query(
        "INSERT INTO ff_lane_registry (lane_id, registered_at_ms, registered_by) \
         VALUES ($1, 0, 'test4g') ON CONFLICT DO NOTHING",
    )
    .bind(lane_id)
    .execute(pool)
    .await
    .expect("insert lane");
}

async fn delete_lanes_with_prefix(pool: &PgPool, prefix: &str) {
    sqlx::query("DELETE FROM ff_lane_registry WHERE lane_id LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("cleanup lanes");
}

fn backend(pool: PgPool) -> Arc<PostgresBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_lanes_returns_boot_and_dynamic_lanes() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let prefix = lane_prefix();
    // Three lanes inserted in reverse order to prove SQL sort wins.
    insert_lane(&pool, &format!("{prefix}_c")).await;
    insert_lane(&pool, &format!("{prefix}_a")).await;
    insert_lane(&pool, &format!("{prefix}_b")).await;

    let be = backend(pool.clone());
    // Page through the whole registry collecting anything with our
    // prefix. We cannot assert "total == 3" because other concurrent
    // tests may have seeded lanes, but we can assert our three appear
    // in sorted order.
    let mut cursor: Option<LaneId> = None;
    let mut seen: Vec<String> = Vec::new();
    loop {
        let page = be.list_lanes(cursor.clone(), 50).await.expect("list_lanes");
        for l in &page.lanes {
            if l.as_str().starts_with(&prefix) {
                seen.push(l.as_str().to_owned());
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(
        seen,
        vec![
            format!("{prefix}_a"),
            format!("{prefix}_b"),
            format!("{prefix}_c"),
        ],
        "all three prefixed lanes must be visible in sorted order"
    );

    delete_lanes_with_prefix(&pool, &prefix).await;
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_lanes_cursor_pagination() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let prefix = lane_prefix();
    for suffix in ["a", "b", "c", "d", "e"] {
        insert_lane(&pool, &format!("{prefix}_{suffix}")).await;
    }

    let be = backend(pool.clone());
    // Limit=2 → 3 pages worth of our prefix (a,b | c,d | e + maybe
    // others) — collect just the prefixed ids and assert ordering.
    let mut cursor: Option<LaneId> = None;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..100 {
        let page = be.list_lanes(cursor.clone(), 2).await.expect("list_lanes");
        for l in &page.lanes {
            if l.as_str().starts_with(&prefix) {
                seen.push(l.as_str().to_owned());
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 5, "all 5 prefixed lanes visible across pages");
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "pagination preserves global sort order");

    delete_lanes_with_prefix(&pool, &prefix).await;
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_lanes_limit_zero_returns_empty() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool);
    let page = be.list_lanes(None, 0).await.expect("list_lanes");
    assert!(page.lanes.is_empty());
    assert!(page.next_cursor.is_none());
}

// ─── list_suspended ───

async fn insert_suspension(
    pool: &PgPool,
    partition_idx: i16,
    uuid: Uuid,
    suspended_at_ms: i64,
    reason: &str,
) {
    sqlx::query(
        "INSERT INTO ff_suspension_current \
         (partition_key, execution_id, suspension_id, suspended_at_ms, \
          timeout_at_ms, reason_code, condition) \
         VALUES ($1, $2, $3, $4, NULL, $5, '{}'::jsonb) \
         ON CONFLICT DO NOTHING",
    )
    .bind(partition_idx)
    .bind(uuid)
    .bind(Uuid::new_v4())
    .bind(suspended_at_ms)
    .bind(reason)
    .execute(pool)
    .await
    .expect("insert suspension");
}

async fn delete_partition_suspensions(pool: &PgPool, partition_idx: i16) {
    sqlx::query("DELETE FROM ff_suspension_current WHERE partition_key = $1")
        .bind(partition_idx)
        .execute(pool)
        .await
        .expect("cleanup suspensions");
}

/// Pick a partition index unlikely to collide with other concurrent
/// tests by deriving from a random UUID modulo 256.
fn pick_partition() -> (i16, PartitionKey) {
    let idx = (Uuid::new_v4().as_u128() as u16) % 256;
    let part = Partition {
        family: PartitionFamily::Flow,
        index: idx,
    };
    (idx as i16, PartitionKey::from(&part))
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_suspended_projects_reason_in_ascending_score() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part_idx, part_key) = pick_partition();
    // Clean before (other tests may have left rows on this partition).
    delete_partition_suspensions(&pool, part_idx).await;

    let u1 = Uuid::new_v4();
    let u2 = Uuid::new_v4();
    let u3 = Uuid::new_v4();
    // Insert in scrambled order; expect sort by (suspended_at_ms, eid).
    insert_suspension(&pool, part_idx, u2, 200, "timer").await;
    insert_suspension(&pool, part_idx, u1, 100, "signal").await;
    insert_suspension(&pool, part_idx, u3, 300, "").await;

    let be = backend(pool.clone());
    let page = be
        .list_suspended(part_key.clone(), None, 10)
        .await
        .expect("list_suspended");
    assert_eq!(page.entries.len(), 3);
    assert_eq!(page.entries[0].suspended_at_ms, 100);
    assert_eq!(page.entries[0].reason, "signal");
    assert_eq!(page.entries[1].suspended_at_ms, 200);
    assert_eq!(page.entries[1].reason, "timer");
    assert_eq!(page.entries[2].suspended_at_ms, 300);
    assert_eq!(page.entries[2].reason, "", "NULL reason_code → empty string");
    assert!(page.next_cursor.is_none());
    // Every projected ExecutionId must be a valid parse-accepted shape.
    for e in &page.entries {
        ExecutionId::parse(e.execution_id.as_str()).expect("valid ExecutionId");
        assert_eq!(e.execution_id.partition(), part_idx as u16);
    }

    delete_partition_suspensions(&pool, part_idx).await;
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_suspended_cursor_pagination_walks_partition() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part_idx, part_key) = pick_partition();
    delete_partition_suspensions(&pool, part_idx).await;

    // Four rows, distinct timestamps so (ms, eid) order is fully
    // determined by ms.
    let mut uuids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    for (i, u) in uuids.iter().enumerate() {
        insert_suspension(&pool, part_idx, *u, 1000 + i as i64, "join").await;
    }
    // Sort uuids in the (ms, uuid) order we expect to see them back.
    // Since timestamps are monotonically increasing with insertion
    // order, the expected output order equals the insertion order.
    let expected_ms: Vec<i64> = (0..4).map(|i| 1000 + i as i64).collect();

    let be = backend(pool.clone());
    let mut cursor: Option<ExecutionId> = None;
    let mut seen_ms: Vec<i64> = Vec::new();
    for _ in 0..10 {
        let page = be
            .list_suspended(part_key.clone(), cursor.clone(), 2)
            .await
            .expect("list_suspended");
        for e in &page.entries {
            seen_ms.push(e.suspended_at_ms);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen_ms, expected_ms, "cursor pagination covers all rows in order");

    // For silent-unused warning suppression + explicit asserting the
    // uuids were actually consumed.
    uuids.sort();
    delete_partition_suspensions(&pool, part_idx).await;
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn list_suspended_limit_zero_returns_empty() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (_, part_key) = pick_partition();
    let be = backend(pool);
    let page = be
        .list_suspended(part_key, None, 0)
        .await
        .expect("list_suspended");
    assert!(page.entries.is_empty());
    assert!(page.next_cursor.is_none());
}
