//! cairn #454 Phase 4a — live-Postgres coverage of the typed
//! `EngineBackend::release_budget` body (option-A per-execution
//! ledger). Parity with the Valkey tests in
//! `crates/ff-backend-valkey/tests/typed_release_budget.rs` (PR #464):
//!
//! 1. A record_spend followed by release returns the aggregate to 0.
//! 2. Multiple record_spend calls accumulate; one release reverses them.
//! 3. Release on an execution that never recorded is an idempotent no-op.
//! 4. Release itself is idempotent (safe to call twice).
//! 5. Releasing one execution does not touch another's attribution.
//!
//! Ignore-gated (live Postgres at `FF_PG_TEST_URL`).

use std::collections::BTreeMap;
use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{CreateBudgetArgs, RecordSpendArgs, ReleaseBudgetArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Row;

const LANE: &str = "release-budget-pg-test-lane";

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

fn backend(pool: PgPool) -> Arc<PostgresBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

fn partition_config() -> PartitionConfig {
    PartitionConfig::default()
}

fn budget_args(bid: &BudgetId) -> CreateBudgetArgs {
    CreateBudgetArgs {
        budget_id: bid.clone(),
        scope_type: "flow".into(),
        scope_id: "flow-1".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into()],
        // High ceiling so multi-spend paths don't breach.
        hard_limits: vec![1_000_000],
        soft_limits: vec![999_999],
        now: TimestampMs(now_ms()),
    }
}

fn spend(
    bid: &BudgetId,
    exec: &ExecutionId,
    tokens: u64,
    idem: &str,
) -> RecordSpendArgs {
    let mut m = BTreeMap::new();
    m.insert("tokens".to_string(), tokens);
    RecordSpendArgs::new(bid.clone(), exec.clone(), m, idem.to_owned())
}

async fn aggregate_tokens(pool: &PgPool, bid: &BudgetId) -> i64 {
    use ff_core::partition::budget_partition;
    let partition_key: i16 = budget_partition(bid, &partition_config()).index as i16;
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key=$1 AND budget_id=$2 AND dimensions_key='tokens'",
    )
    .bind(partition_key)
    .bind(bid.to_string())
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|r| r.get::<i64, _>("current_value")).unwrap_or(0)
}

async fn ledger_row_count(pool: &PgPool, bid: &BudgetId, exec: &ExecutionId) -> i64 {
    use ff_core::partition::budget_partition;
    let partition_key: i16 = budget_partition(bid, &partition_config()).index as i16;
    let s = exec.as_str();
    let uuid_str = &s[s.rfind("}:").unwrap() + 2..];
    let exec_uuid = uuid::Uuid::parse_str(uuid_str).unwrap();
    let row = sqlx::query(
        "SELECT COUNT(*) AS c FROM ff_budget_usage_by_exec \
         WHERE partition_key=$1 AND budget_id=$2 AND execution_id=$3",
    )
    .bind(partition_key)
    .bind(bid.to_string())
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .unwrap();
    row.get::<i64, _>("c")
}

/// 1 — record 5 tokens, release, aggregate back to 0, ledger gone.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_after_record_zeros_aggregate() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be.create_budget(budget_args(&bid)).await.expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = be
        .record_spend(spend(&bid, &exec, 5, "r-1"))
        .await
        .expect("record");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 5);
    assert_eq!(ledger_row_count(&pool, &bid, &exec).await, 1);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 0);
    assert_eq!(ledger_row_count(&pool, &bid, &exec).await, 0);
}

/// 2 — multiple record_spend calls accumulate on the ledger; one
/// release reverses the full accumulation.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_reverses_accumulated_spends() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be.create_budget(budget_args(&bid)).await.expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = be.record_spend(spend(&bid, &exec, 3, "a")).await.expect("a");
    let _ = be.record_spend(spend(&bid, &exec, 7, "b")).await.expect("b");
    let _ = be.record_spend(spend(&bid, &exec, 2, "c")).await.expect("c");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 12);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 0);
    assert_eq!(ledger_row_count(&pool, &bid, &exec).await, 0);
}

/// 3 — release on an execution that never recorded is an idempotent
/// no-op (aggregate untouched).
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_without_prior_record_is_noop() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be.create_budget(budget_args(&bid)).await.expect("create_budget");

    // Seed the aggregate with a DIFFERENT execution so we can prove
    // the no-op doesn't touch it.
    let seeded = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = be
        .record_spend(spend(&bid, &seeded, 9, "seed"))
        .await
        .expect("seed");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 9);

    let ghost = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), ghost))
        .await
        .expect("noop release");
    // Aggregate unchanged.
    assert_eq!(aggregate_tokens(&pool, &bid).await, 9);
}

/// 4 — release is itself idempotent: calling twice leaves aggregate
/// at 0 and is safe.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_is_idempotent() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be.create_budget(budget_args(&bid)).await.expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = be
        .record_spend(spend(&bid, &exec, 4, "x"))
        .await
        .expect("record");
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release 1");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 0);

    // Second release — no-op.
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release 2 (noop)");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 0);
}

/// 5 — releasing one execution does not touch another's attribution.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_isolated_between_executions() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be.create_budget(budget_args(&bid)).await.expect("create_budget");

    let e1 = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let e2 = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = be.record_spend(spend(&bid, &e1, 5, "e1")).await.expect("e1");
    let _ = be.record_spend(spend(&bid, &e2, 8, "e2")).await.expect("e2");
    assert_eq!(aggregate_tokens(&pool, &bid).await, 13);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), e1.clone()))
        .await
        .expect("release e1");
    // Only e1's 5 reversed; e2's 8 stays.
    assert_eq!(aggregate_tokens(&pool, &bid).await, 8);
    assert_eq!(ledger_row_count(&pool, &bid, &e1).await, 0);
    assert_eq!(ledger_row_count(&pool, &bid, &e2).await, 1);
}
