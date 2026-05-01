//! cairn #454 Phase 5 — SQLite coverage of the typed
//! `EngineBackend::release_budget` body. Parity with the PG tests at
//! `ff-backend-postgres/tests/typed_release_budget.rs`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{CreateBudgetArgs, RecordSpendArgs, ReleaseBudgetArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};
use serial_test::serial;
use sqlx::Row;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const LANE: &str = "release-budget-sqlite-test-lane";

fn now_ms() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn uuid_like() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:typed-release-budget-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("construct")
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
        hard_limits: vec![1_000_000],
        soft_limits: vec![999_999],
        now: TimestampMs(now_ms()),
    }
}

fn spend(bid: &BudgetId, exec: &ExecutionId, tokens: u64, idem: &str) -> RecordSpendArgs {
    let mut m = BTreeMap::new();
    m.insert("tokens".to_string(), tokens);
    RecordSpendArgs::new(bid.clone(), exec.clone(), m, idem.to_owned())
}

async fn aggregate_tokens(be: &SqliteBackend, bid: &BudgetId) -> i64 {
    let row = sqlx::query(
        "SELECT current_value FROM ff_budget_usage \
         WHERE partition_key=0 AND budget_id=?1 AND dimensions_key='tokens'",
    )
    .bind(bid.to_string())
    .fetch_optional(be.pool_for_test())
    .await
    .unwrap();
    row.map(|r| r.get::<i64, _>("current_value")).unwrap_or(0)
}

async fn ledger_row_count(be: &SqliteBackend, bid: &BudgetId, exec: &ExecutionId) -> i64 {
    let s = exec.as_str();
    let uuid_str = &s[s.rfind("}:").unwrap() + 2..];
    let row = sqlx::query(
        "SELECT COUNT(*) AS c FROM ff_budget_usage_by_exec \
         WHERE partition_key=0 AND budget_id=?1 AND execution_id=?2",
    )
    .bind(bid.to_string())
    .bind(uuid_str)
    .fetch_one(be.pool_for_test())
    .await
    .unwrap();
    row.get::<i64, _>("c")
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn release_after_record_zeros_aggregate() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb.create_budget(budget_args(&bid)).await.expect("create");
    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = eb
        .record_spend(spend(&bid, &exec, 5, "r-1"))
        .await
        .expect("record");
    assert_eq!(aggregate_tokens(&be, &bid).await, 5);
    assert_eq!(ledger_row_count(&be, &bid, &exec).await, 1);

    eb.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release");
    assert_eq!(aggregate_tokens(&be, &bid).await, 0);
    assert_eq!(ledger_row_count(&be, &bid, &exec).await, 0);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn release_reverses_accumulated_spends() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb.create_budget(budget_args(&bid)).await.expect("create");
    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = eb.record_spend(spend(&bid, &exec, 3, "a")).await.unwrap();
    let _ = eb.record_spend(spend(&bid, &exec, 7, "b")).await.unwrap();
    let _ = eb.record_spend(spend(&bid, &exec, 2, "c")).await.unwrap();
    assert_eq!(aggregate_tokens(&be, &bid).await, 12);

    eb.release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await
        .expect("release");
    assert_eq!(aggregate_tokens(&be, &bid).await, 0);
    assert_eq!(ledger_row_count(&be, &bid, &exec).await, 0);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn release_without_prior_record_is_noop() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb.create_budget(budget_args(&bid)).await.expect("create");
    let seeded = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = eb
        .record_spend(spend(&bid, &seeded, 9, "seed"))
        .await
        .unwrap();
    assert_eq!(aggregate_tokens(&be, &bid).await, 9);

    let ghost = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    eb.release_budget(ReleaseBudgetArgs::new(bid.clone(), ghost))
        .await
        .expect("noop release");
    assert_eq!(aggregate_tokens(&be, &bid).await, 9);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn release_is_idempotent_and_isolated() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb.create_budget(budget_args(&bid)).await.expect("create");

    let e1 = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let e2 = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = eb.record_spend(spend(&bid, &e1, 5, "e1")).await.unwrap();
    let _ = eb.record_spend(spend(&bid, &e2, 8, "e2")).await.unwrap();
    assert_eq!(aggregate_tokens(&be, &bid).await, 13);

    eb.release_budget(ReleaseBudgetArgs::new(bid.clone(), e1.clone()))
        .await
        .expect("release e1");
    assert_eq!(aggregate_tokens(&be, &bid).await, 8);
    assert_eq!(ledger_row_count(&be, &bid, &e1).await, 0);
    assert_eq!(ledger_row_count(&be, &bid, &e2).await, 1);

    // Second release is a no-op.
    eb.release_budget(ReleaseBudgetArgs::new(bid.clone(), e1.clone()))
        .await
        .expect("release e1 again");
    assert_eq!(aggregate_tokens(&be, &bid).await, 8);
}
