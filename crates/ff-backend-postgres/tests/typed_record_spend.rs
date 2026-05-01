//! cairn #454 Phase 4a — live-Postgres coverage of the typed
//! `EngineBackend::record_spend` body. Parity with the Valkey tests in
//! `crates/ff-backend-valkey/tests/typed_record_spend.rs` (PR #464):
//!
//! 1. `Ok` on first application under the hard limit (mirror to
//!    `ff_budget_usage_by_exec`).
//! 2. `SoftBreach` when the increment crosses the soft limit (still
//!    applied — soft is advisory).
//! 3. `HardBreach` when the increment would cross the hard limit
//!    (rejected — counter + ledger unchanged).
//! 4. `AlreadyApplied` on a replay with the same `idempotency_key`.
//!
//! Ignore-gated (live Postgres at `FF_PG_TEST_URL`) following
//! `pg_budget_admin.rs`. Run via:
//!
//! ```text
//! FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres \
//!     --test typed_record_spend -- --ignored --test-threads=1
//! ```
//!
//! CI gates run with `FF_PG_TEST_URL` set; local sandboxed runs without
//! PG just print "skipped" via `setup_or_skip`.

use std::collections::BTreeMap;
use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    CreateBudgetArgs, CreateBudgetResult, RecordSpendArgs, ReportUsageResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const LANE: &str = "record-spend-pg-test-lane";

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

/// Build a `CreateBudgetArgs` with the provided `(dim, hard, soft)`
/// triples. Matches the shape used in `pg_budget_admin.rs`.
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

async fn ledger_delta(pool: &PgPool, bid: &BudgetId, eid: &ExecutionId, dim: &str) -> Option<i64> {
    use ff_core::partition::budget_partition;
    use sqlx::Row;
    let partition_key: i16 = budget_partition(bid, &partition_config()).index as i16;
    let s = eid.as_str();
    let uuid_str = &s[s.rfind("}:").unwrap() + 2..];
    let exec_uuid = uuid::Uuid::parse_str(uuid_str).unwrap();
    let row = sqlx::query(
        "SELECT delta_total FROM ff_budget_usage_by_exec \
         WHERE partition_key=$1 AND budget_id=$2 AND execution_id=$3 AND dimensions_key=$4",
    )
    .bind(partition_key)
    .bind(bid.to_string())
    .bind(exec_uuid)
    .bind(dim)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|r| r.get::<i64, _>("delta_total"))
}

/// Path 1 — happy-path `Ok` under the hard limit with ledger mirror.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn record_spend_ok_path() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let r = be
        .create_budget(budget_args(
            &bid,
            &[("tokens", 100, 50), ("cost_cents", 1000, 500)],
        ))
        .await
        .expect("create_budget");
    assert!(matches!(r, CreateBudgetResult::Created { .. }));

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let r = be
        .record_spend(spend_args(
            &bid,
            &exec,
            &[("tokens", 10), ("cost_cents", 20)],
            "ok-1",
        ))
        .await
        .expect("record_spend");
    assert!(matches!(r, ReportUsageResult::Ok), "got {r:?}");

    // Ledger mirrored.
    assert_eq!(ledger_delta(&pool, &bid, &exec, "tokens").await, Some(10));
    assert_eq!(ledger_delta(&pool, &bid, &exec, "cost_cents").await, Some(20));
}

/// Path 2 — idempotent replay returns `AlreadyApplied` and does NOT
/// double-increment aggregate or ledger.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn record_spend_idempotent_replay() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    let args = spend_args(&bid, &exec, &[("tokens", 7)], "replay-me");
    let r = be.record_spend(args.clone()).await.expect("first");
    assert!(matches!(r, ReportUsageResult::Ok), "got {r:?}");

    let r2 = be.record_spend(args).await.expect("replay");
    assert!(matches!(r2, ReportUsageResult::AlreadyApplied), "got {r2:?}");

    // Ledger unchanged (still 7, not 14).
    assert_eq!(ledger_delta(&pool, &bid, &exec, "tokens").await, Some(7));
}

/// Path 3 — soft-breach applies the increment AND reports the breach.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn record_spend_soft_breach() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    // current=0, +60 = 60 > soft(50), still < hard(100).
    let r = be
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
    // Ledger reflects the applied increment.
    assert_eq!(ledger_delta(&pool, &bid, &exec, "tokens").await, Some(60));
}

/// Path 4 — hard-breach does NOT apply the increment and does NOT
/// touch the ledger.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn record_spend_hard_breach() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let be = backend(pool.clone());
    let bid = BudgetId::new();
    let _ = be
        .create_budget(budget_args(&bid, &[("tokens", 100, 50)]))
        .await
        .expect("create_budget");

    let exec = ExecutionId::solo(&LaneId::new(LANE), &partition_config());
    // Seed to 40.
    let _ = be
        .record_spend(spend_args(&bid, &exec, &[("tokens", 40)], "hard-seed"))
        .await
        .expect("seed");

    // +80 = 120 > hard(100) — reject.
    let r = be
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
    assert_eq!(ledger_delta(&pool, &bid, &exec, "tokens").await, Some(40));
}
