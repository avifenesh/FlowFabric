//! cairn #454 Phase 5 — SQLite coverage of the typed
//! `EngineBackend::record_spend` body. Parity with the PG tests at
//! `ff-backend-postgres/tests/typed_record_spend.rs`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{
    CreateBudgetArgs, CreateBudgetResult, RecordSpendArgs, ReportUsageResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};
use serial_test::serial;
use sqlx::Row;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const LANE: &str = "record-spend-sqlite-test-lane";

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
        "file:typed-record-spend-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("construct")
}

fn partition_config() -> PartitionConfig {
    PartitionConfig::default()
}

fn budget_args(bid: &BudgetId, dims: &[(&str, u64, u64)]) -> CreateBudgetArgs {
    CreateBudgetArgs {
        budget_id: bid.clone(),
        scope_type: "flow".into(),
        scope_id: "flow-1".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: dims.iter().map(|(d, _, _)| (*d).to_string()).collect(),
        hard_limits: dims.iter().map(|(_, h, _)| *h).collect(),
        soft_limits: dims.iter().map(|(_, _, s)| *s).collect(),
        now: TimestampMs(now_ms()),
    }
}

fn spend_args(
    bid: &BudgetId,
    exec: &ExecutionId,
    deltas: &[(&str, u64)],
    idem: &str,
) -> RecordSpendArgs {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in deltas {
        m.insert((*k).to_owned(), *v);
    }
    RecordSpendArgs::new(bid.clone(), exec.clone(), m, idem.to_owned())
}

async fn ledger_delta(
    backend: &SqliteBackend,
    bid: &BudgetId,
    eid: &ExecutionId,
    dim: &str,
) -> Option<i64> {
    let s = eid.as_str();
    let uuid_str = &s[s.rfind("}:").unwrap() + 2..];
    let row = sqlx::query(
        "SELECT delta_total FROM ff_budget_usage_by_exec \
         WHERE partition_key=0 AND budget_id=?1 AND execution_id=?2 AND dimensions_key=?3",
    )
    .bind(bid.to_string())
    .bind(uuid_str)
    .bind(dim)
    .fetch_optional(backend.pool_for_test())
    .await
    .unwrap();
    row.map(|r| r.get::<i64, _>("delta_total"))
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn record_spend_ok_path() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let r = eb
        .create_budget(budget_args(
            &bid,
            &[("tokens", 100, 50), ("cost_cents", 1000, 500)],
        ))
        .await
        .expect("create_budget");
    assert!(matches!(r, CreateBudgetResult::Created { .. }));

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let r = eb
        .record_spend(spend_args(
            &bid,
            &exec,
            &[("tokens", 10), ("cost_cents", 20)],
            "ok-1",
        ))
        .await
        .expect("record_spend");
    assert!(matches!(r, ReportUsageResult::Ok), "got {r:?}");

    assert_eq!(ledger_delta(&be, &bid, &exec, "tokens").await, Some(10));
    assert_eq!(ledger_delta(&be, &bid, &exec, "cost_cents").await, Some(20));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn record_spend_idempotent_replay() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");
    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let args = spend_args(&bid, &exec, &[("tokens", 7)], "replay-me");
    let r = eb.record_spend(args.clone()).await.expect("first");
    assert!(matches!(r, ReportUsageResult::Ok), "got {r:?}");

    let r2 = eb.record_spend(args).await.expect("replay");
    assert!(
        matches!(r2, ReportUsageResult::AlreadyApplied),
        "got {r2:?}"
    );
    assert_eq!(ledger_delta(&be, &bid, &exec, "tokens").await, Some(7));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn record_spend_soft_breach() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let r = eb
        .record_spend(spend_args(&bid, &exec, &[("tokens", 60)], "soft-1"))
        .await
        .expect("soft");
    match r {
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 60);
            assert_eq!(soft_limit, 50);
        }
        other => panic!("expected SoftBreach, got {other:?}"),
    }
    assert_eq!(ledger_delta(&be, &bid, &exec, "tokens").await, Some(60));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn record_spend_hard_breach() {
    let be = fresh_backend().await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let bid = BudgetId::new();
    let _ = eb
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let _ = eb
        .record_spend(spend_args(&bid, &exec, &[("tokens", 40)], "hard-seed"))
        .await
        .expect("seed");

    let r = eb
        .record_spend(spend_args(&bid, &exec, &[("tokens", 80)], "hard-1"))
        .await
        .expect("hard");
    match r {
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 40);
            assert_eq!(hard_limit, 100);
        }
        other => panic!("expected HardBreach, got {other:?}"),
    }
    // Ledger still at the seed value — 40, not 120.
    assert_eq!(ledger_delta(&be, &bid, &exec, "tokens").await, Some(40));
}
