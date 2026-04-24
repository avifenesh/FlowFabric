//! Wave 4f integration tests — `EngineBackend::report_usage` on the
//! Postgres backend.
//!
//! Runs under the same `FF_PG_TEST_URL` harness as
//! `ff-backend-postgres/tests/schema_0002.rs`: the suite is
//! `#[ignore]`'d and skipped when the env var is unset, and
//! `apply_migrations` is idempotent so each test gets the Wave 3b
//! schema (`0001_initial.sql` + `0002_budget.sql`) in place.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4f_test \
//!   cargo test -p ff-test --test pg_engine_backend_budget -- --ignored
//! ```
//!
//! Each test uses a **fresh random `BudgetId`** so the 256-way hash
//! partitioning naturally avoids cross-test interference without
//! requiring a per-test database.
//!
//! Verification focus (RFC-v0.7 Wave 4f acceptance):
//! * admit-path happy
//! * dedup replay returns the cached outcome without double-incrementing
//! * hard-limit breach returns `HardBreach` and does NOT apply increments
//! * soft-limit breach returns `SoftBreach` and DOES apply increments
//! * `upsert_policy_for_test` round-trips into `report_usage` (stand-in
//!   for `update_budget_policy` — NOT on the trait in v0.7).
//! * `ff_budget_usage_dedup` row count grows by exactly 1 per distinct
//!   `dedup_key`.

#![allow(clippy::bool_assert_comparison)]

use std::collections::BTreeMap;

use ff_backend_postgres::{apply_migrations, budget::upsert_policy_for_test, PostgresBackend};
use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque, UsageDimensions};
use ff_core::contracts::ReportUsageResult;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{budget_partition, PartitionConfig};
use ff_core::types::BudgetId;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// Acquire a pool + install the Wave 3/3b schemas, or return `None`
/// so the test bails out cleanly in environments without Postgres.
async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&pool).await.expect("apply_migrations clean");
    Some(pool)
}

/// Build a `PostgresBackend` from the pool so the tests speak the
/// real trait surface.
fn backend(pool: &PgPool) -> std::sync::Arc<PostgresBackend> {
    PostgresBackend::from_pool(pool.clone(), PartitionConfig::default())
}

/// Synthetic handle — `report_usage` on the Postgres backend does not
/// yet thread lease validation, so any well-formed Handle is fine.
fn fake_handle() -> Handle {
    Handle::new(
        BackendTag::Postgres,
        HandleKind::Fresh,
        HandleOpaque::new(Box::from(&b"wave4f-test"[..])),
    )
}

/// Count the dedup rows for a budget's partition (all tests seed
/// unique BudgetId so the partition's dedup row count cleanly traces
/// back to this test's calls — modulo 1/256 hash collisions, which
/// the assertions compensate for with relative deltas).
async fn dedup_row_count(pool: &PgPool, budget: &BudgetId) -> i64 {
    let partition = budget_partition(budget, &PartitionConfig::default());
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM ff_budget_usage_dedup \
         WHERE partition_key = $1 AND dedup_key LIKE $2",
    )
    .bind(partition.index as i16)
    .bind(format!("{}%", budget))
    .fetch_one(pool)
    .await
    .unwrap();
    row.get("n")
}

fn dims(custom: &[(&str, u64)], dedup: Option<&str>) -> UsageDimensions {
    let mut map: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in custom {
        map.insert((*k).to_string(), *v);
    }
    let mut d = UsageDimensions::new();
    d.custom = map;
    if let Some(k) = dedup {
        d = d.with_dedup_key(k.to_string());
    }
    d
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_admit_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let be = backend(&pool);
    let budget = BudgetId::new();

    // No policy set ⇒ no limits ⇒ report succeeds and applies the
    // increment.
    let out = be
        .report_usage(&fake_handle(), &budget, dims(&[("tokens", 10)], None))
        .await
        .expect("report_usage");
    assert_eq!(out, ReportUsageResult::Ok);

    // Verify current_value = 10.
    let partition = budget_partition(&budget, &PartitionConfig::default());
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3",
    )
    .bind(partition.index as i16)
    .bind(budget.to_string())
    .bind("tokens")
    .fetch_one(&pool)
    .await
    .unwrap();
    let cur: i64 = row.get("current_value");
    assert_eq!(cur, 10);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_dedup_replay_does_not_double_increment() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(&pool);
    let budget = BudgetId::new();
    let dk = format!("{}-call-1", budget);

    let before = dedup_row_count(&pool, &budget).await;

    // First call: applies increment, seeds dedup row.
    let first = be
        .report_usage(
            &fake_handle(),
            &budget,
            dims(&[("tokens", 5)], Some(&dk)),
        )
        .await
        .expect("first report_usage");
    assert_eq!(first, ReportUsageResult::Ok);

    // Second call with the SAME dedup_key: must replay first outcome
    // and NOT touch usage.
    let second = be
        .report_usage(
            &fake_handle(),
            &budget,
            dims(&[("tokens", 5)], Some(&dk)),
        )
        .await
        .expect("replay report_usage");
    assert_eq!(second, ReportUsageResult::Ok);

    // Usage stays at 5 (not 10).
    let partition = budget_partition(&budget, &PartitionConfig::default());
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3",
    )
    .bind(partition.index as i16)
    .bind(budget.to_string())
    .bind("tokens")
    .fetch_one(&pool)
    .await
    .unwrap();
    let cur: i64 = row.get("current_value");
    assert_eq!(cur, 5, "replay must not double-increment");

    // Exactly one dedup row landed for this budget.
    let after = dedup_row_count(&pool, &budget).await;
    assert_eq!(after - before, 1, "dedup row count grows by exactly 1 per distinct dedup_key");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_hard_limit_breach_rejects_without_applying() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(&pool);
    let budget = BudgetId::new();

    // Seed a hard limit of 100 on `tokens`.
    upsert_policy_for_test(
        &pool,
        &PartitionConfig::default(),
        &budget,
        json!({"hard_limits": {"tokens": 100}}),
    )
    .await
    .expect("seed policy");

    // Get to 90.
    be.report_usage(&fake_handle(), &budget, dims(&[("tokens", 90)], None))
        .await
        .expect("report 90");

    // +20 would exceed 100 — HardBreach, no application.
    let out = be
        .report_usage(&fake_handle(), &budget, dims(&[("tokens", 20)], None))
        .await
        .expect("report 20 hitting hard limit");
    match out {
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 90);
            assert_eq!(hard_limit, 100);
        }
        other => panic!("expected HardBreach, got {other:?}"),
    }

    // Usage must still be 90.
    let partition = budget_partition(&budget, &PartitionConfig::default());
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3",
    )
    .bind(partition.index as i16)
    .bind(budget.to_string())
    .bind("tokens")
    .fetch_one(&pool)
    .await
    .unwrap();
    let cur: i64 = row.get("current_value");
    assert_eq!(cur, 90, "HardBreach must not apply increments");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_soft_limit_breach_is_advisory() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(&pool);
    let budget = BudgetId::new();

    // Soft=50, hard=1000 on `tokens`.
    upsert_policy_for_test(
        &pool,
        &PartitionConfig::default(),
        &budget,
        json!({"soft_limits": {"tokens": 50}, "hard_limits": {"tokens": 1000}}),
    )
    .await
    .expect("seed policy");

    let out = be
        .report_usage(&fake_handle(), &budget, dims(&[("tokens", 75)], None))
        .await
        .expect("report 75 past soft");
    match out {
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 75);
            assert_eq!(soft_limit, 50);
        }
        other => panic!("expected SoftBreach, got {other:?}"),
    }

    // Soft breach must HAVE applied the increment.
    let partition = budget_partition(&budget, &PartitionConfig::default());
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3",
    )
    .bind(partition.index as i16)
    .bind(budget.to_string())
    .bind("tokens")
    .fetch_one(&pool)
    .await
    .unwrap();
    let cur: i64 = row.get("current_value");
    assert_eq!(cur, 75, "SoftBreach must still apply the increment");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn policy_upsert_roundtrip_visible_in_report_usage() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(&pool);
    let budget = BudgetId::new();

    // Initially no policy → Ok even on a big increment.
    let out1 = be
        .report_usage(&fake_handle(), &budget, dims(&[("tokens", 10_000)], None))
        .await
        .expect("no-policy report");
    assert_eq!(out1, ReportUsageResult::Ok);

    // Now install a policy with hard_limit=1. Subsequent report must
    // see it (HardBreach because current_value already 10_000).
    upsert_policy_for_test(
        &pool,
        &PartitionConfig::default(),
        &budget,
        json!({"hard_limits": {"tokens": 1}}),
    )
    .await
    .expect("install policy");

    let out2 = be
        .report_usage(&fake_handle(), &budget, dims(&[("tokens", 1)], None))
        .await
        .expect("post-policy report");
    assert!(
        matches!(out2, ReportUsageResult::HardBreach { .. }),
        "post-policy report must see the new hard_limit: got {out2:?}",
    );
}
