//! RFC-023 Phase 3.4 — Wave 9 budget/quota admin integration tests.
//!
//! Covers the 5 admin methods + the `report_usage_impl` breach-counter
//! extension on the SQLite backend:
//!
//!   * `create_budget` — happy path + idempotent second call.
//!   * `reset_budget` — zeroes counters + advances `next_reset_at_ms`.
//!   * `create_quota_policy` — writes `ff_quota_policy`; the two
//!     companion tables (`ff_quota_window`, `ff_quota_admitted`) exist
//!     from migration 0012 but are not populated on create (matches PG
//!     reference).
//!   * `get_budget_status` — returns limits + counters + `created_at`.
//!   * `report_usage_admin` — increments usage + counters (Ok, soft
//!     breach, hard breach branches).
//!
//! Hard-breach branch verifies that breach_count increments AND the
//! increment is NOT applied (Valkey-canonical strict-mode semantics).

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{
    CreateBudgetArgs, CreateBudgetResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    ReportUsageAdminArgs, ReportUsageResult, ResetBudgetArgs, ResetBudgetResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::{BudgetId, QuotaPolicyId, TimestampMs};
use serial_test::serial;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    // SAFETY: test-only env; serial-gated.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-wave9-budget-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("backend")
}

fn budget_args(
    id: &BudgetId,
    reset_interval_ms: u64,
    hard_tokens: u64,
    soft_tokens: u64,
) -> CreateBudgetArgs {
    CreateBudgetArgs {
        budget_id: id.clone(),
        scope_type: "flow".into(),
        scope_id: "flow-1".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![hard_tokens],
        soft_limits: vec![soft_tokens],
        now: TimestampMs(now_ms()),
    }
}

fn admin_args(dim: &str, delta: u64) -> ReportUsageAdminArgs {
    ReportUsageAdminArgs::new(vec![dim.into()], vec![delta], TimestampMs(now_ms()))
}

// ── create_budget ──────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn create_budget_happy_path() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();

    let r1 = b.create_budget(budget_args(&bid, 0, 100, 80)).await.unwrap();
    assert!(matches!(r1, CreateBudgetResult::Created { .. }));

    let r2 = b.create_budget(budget_args(&bid, 0, 100, 80)).await.unwrap();
    assert!(matches!(r2, CreateBudgetResult::AlreadySatisfied { .. }));
}

#[tokio::test]
#[serial]
async fn create_budget_seeds_next_reset_with_interval() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    let before = now_ms();

    let interval_ms: u64 = 60_000;
    b.create_budget(budget_args(&bid, interval_ms, 100, 80))
        .await
        .unwrap();

    let status = b.get_budget_status(&bid).await.unwrap();
    let next: i64 = status
        .next_reset_at
        .as_deref()
        .expect("next_reset_at set when interval > 0")
        .parse()
        .expect("parseable");
    assert!(
        next >= before + interval_ms as i64,
        "next_reset_at must be at least now + interval"
    );
}

// ── reset_budget ───────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn reset_budget_clears_counters_advances_next_reset() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    let interval_ms: u64 = 60_000;
    b.create_budget(budget_args(&bid, interval_ms, 100, 80))
        .await
        .unwrap();
    b.report_usage_admin(&bid, admin_args("tokens", 40))
        .await
        .unwrap();

    let pre = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(pre.usage.get("tokens").copied(), Some(40));

    let t = now_ms();
    let r = b
        .reset_budget(ResetBudgetArgs {
            budget_id: bid.clone(),
            now: TimestampMs(t),
        })
        .await
        .unwrap();
    let next = match r {
        ResetBudgetResult::Reset { next_reset_at } => next_reset_at.0,
        #[allow(unreachable_patterns)]
        _ => panic!("unexpected reset result"),
    };
    assert!(next >= t + interval_ms as i64);

    let post = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(post.usage.get("tokens").copied(), Some(0));
    assert!(post.last_breach_at.is_none());
    assert!(post.last_breach_dim.is_none());
}

#[tokio::test]
#[serial]
async fn reset_budget_not_found_returns_notfound() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    let err = b
        .reset_budget(ResetBudgetArgs {
            budget_id: bid,
            now: TimestampMs(now_ms()),
        })
        .await
        .unwrap_err();
    match err {
        EngineError::NotFound { entity } => assert_eq!(entity, "budget"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ── create_quota_policy ────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn create_quota_policy_writes_policy_row_idempotent() {
    let b = fresh_backend().await;
    let qid = QuotaPolicyId::new();
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs(now_ms()),
    };
    let r1 = b.create_quota_policy(args.clone()).await.unwrap();
    assert!(matches!(r1, CreateQuotaPolicyResult::Created { .. }));
    let r2 = b.create_quota_policy(args).await.unwrap();
    assert!(matches!(r2, CreateQuotaPolicyResult::AlreadySatisfied { .. }));
}

// ── get_budget_status ──────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn get_budget_status_returns_limits_and_counters() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();

    let missing = b.get_budget_status(&bid).await.unwrap_err();
    match missing {
        EngineError::NotFound { entity } => assert_eq!(entity, "budget"),
        other => panic!("expected NotFound, got {other:?}"),
    }

    b.create_budget(budget_args(&bid, 0, 100, 80)).await.unwrap();
    let status = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(status.scope_type, "flow");
    assert_eq!(status.scope_id, "flow-1");
    assert_eq!(status.enforcement_mode, "strict");
    assert_eq!(status.hard_limits.get("tokens").copied(), Some(100));
    assert_eq!(status.soft_limits.get("tokens").copied(), Some(80));
    assert_eq!(status.breach_count, 0);
    assert_eq!(status.soft_breach_count, 0);
    assert!(status.created_at.is_some());
    assert!(status.next_reset_at.is_none(), "no reset scheduled");
}

// ── report_usage_admin ─────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn report_usage_admin_ok_increments_usage() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    b.create_budget(budget_args(&bid, 0, 100, 80)).await.unwrap();

    let r = b.report_usage_admin(&bid, admin_args("tokens", 30)).await.unwrap();
    assert!(matches!(r, ReportUsageResult::Ok));
    let status = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(status.usage.get("tokens").copied(), Some(30));
    assert_eq!(status.breach_count, 0);
    assert_eq!(status.soft_breach_count, 0);
}

#[tokio::test]
#[serial]
async fn report_usage_admin_soft_breach_increments_soft_count() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    b.create_budget(budget_args(&bid, 0, 100, 50)).await.unwrap();

    // 60 > soft 50, but <= hard 100 ⇒ soft breach, increment applied.
    let r = b.report_usage_admin(&bid, admin_args("tokens", 60)).await.unwrap();
    assert!(
        matches!(r, ReportUsageResult::SoftBreach { .. }),
        "expected SoftBreach, got {r:?}"
    );
    let status = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(status.usage.get("tokens").copied(), Some(60));
    assert_eq!(status.soft_breach_count, 1);
    assert_eq!(status.breach_count, 0);
}

#[tokio::test]
#[serial]
async fn report_usage_admin_hard_breach_rejects_and_increments_hard_count() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    b.create_budget(budget_args(&bid, 0, 100, 50)).await.unwrap();

    // First, 60 → soft breach (current 60).
    b.report_usage_admin(&bid, admin_args("tokens", 60))
        .await
        .unwrap();

    // Now 60 more would push to 120 > hard 100 ⇒ hard breach, reject.
    let r = b.report_usage_admin(&bid, admin_args("tokens", 60)).await.unwrap();
    assert!(
        matches!(r, ReportUsageResult::HardBreach { .. }),
        "expected HardBreach, got {r:?}"
    );
    let status = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(
        status.usage.get("tokens").copied(),
        Some(60),
        "hard-breach must NOT apply the increment"
    );
    assert_eq!(status.breach_count, 1);
    assert_eq!(status.last_breach_dim.as_deref(), Some("tokens"));
}

/// Token-budget-style scenario (mirrors `examples/token-budget`
/// trip logic): batch of inferences reports usage; enforcement loop
/// polls `get_budget_status` and trips on either `breach_count > 0`
/// or hard-limit usage. Exercises the full Wave-9 admin surface
/// end-to-end at the trait-level, standing in for the live-server
/// run that Phase 3.5 (scanner supervisor + scheduler wiring) will
/// unblock.
#[tokio::test]
#[serial]
async fn token_budget_scenario_trips_hard_breach_via_counter() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();

    // Mirror examples/token-budget parameters: two dims, strict mode.
    let args = CreateBudgetArgs {
        budget_id: bid.clone(),
        scope_type: "flow".into(),
        scope_id: "flow-abc".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into(), "cost_micros".into()],
        hard_limits: vec![1_200, 15_000],
        soft_limits: vec![800, 10_000],
        now: TimestampMs(now_ms()),
    };
    b.create_budget(args).await.unwrap();

    // Simulate a batch of 5 inferences at ~350 tokens each = 1750 tokens,
    // which will soft-trip around batch 3 and hard-trip around batch 4.
    let mut soft_seen = false;
    let mut hard_seen = false;
    for i in 0..5 {
        let args = ReportUsageAdminArgs::new(
            vec!["tokens".into(), "cost_micros".into()],
            vec![350, 350 * 25 / 1000],
            TimestampMs(now_ms()),
        )
        .with_dedup_key(format!("attempt-{i}"));
        let r = b.report_usage_admin(&bid, args).await.unwrap();
        match r {
            ReportUsageResult::Ok => {}
            ReportUsageResult::SoftBreach { .. } => soft_seen = true,
            ReportUsageResult::HardBreach { .. } => {
                hard_seen = true;
                break;
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert!(soft_seen, "expected at least one SoftBreach in the batch");
    assert!(hard_seen, "expected a HardBreach to trip");

    let status = b.get_budget_status(&bid).await.unwrap();
    assert!(status.breach_count >= 1, "breach_count must increment");
    assert!(
        status.soft_breach_count >= 1,
        "soft_breach_count must increment before hard trip"
    );
    // Token-budget example's enforcement loop trips on either hard-
    // limit usage OR breach_count > 0. Assert the latter path works.
    let tokens = status.usage.get("tokens").copied().unwrap_or(0);
    assert!(
        tokens < 1_200,
        "hard breach must reject the triggering increment — got {tokens}"
    );
}

#[tokio::test]
#[serial]
async fn report_usage_admin_dedup_replay_returns_cached_outcome() {
    let b = fresh_backend().await;
    let bid = BudgetId::new();
    b.create_budget(budget_args(&bid, 0, 100, 80)).await.unwrap();

    let dk = format!("dedup-{}", uuid_like());
    let args = ReportUsageAdminArgs::new(
        vec!["tokens".into()],
        vec![10],
        TimestampMs(now_ms()),
    )
    .with_dedup_key(dk.clone());

    let r1 = b.report_usage_admin(&bid, args.clone()).await.unwrap();
    assert!(matches!(r1, ReportUsageResult::Ok));

    // Replay with the same dedup_key — the cached outcome returns and
    // the increment is NOT applied again.
    let r2 = b.report_usage_admin(&bid, args).await.unwrap();
    assert!(matches!(r2, ReportUsageResult::Ok));

    let status = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(
        status.usage.get("tokens").copied(),
        Some(10),
        "dedup replay must not double-count"
    );
}
