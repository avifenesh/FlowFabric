//! RFC-020 Wave 9 Standalone-1 integration tests.
//!
//! Exercises `create_budget`, `reset_budget`, `create_quota_policy`,
//! `get_budget_status`, `report_usage_admin` trait methods + the
//! `budget_reset` reconciler against a live Postgres with migrations
//! 0012 + 0013 applied.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres \
//!     --test pg_budget_admin -- --ignored
//! ```

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    CreateBudgetArgs, CreateBudgetResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    ReportUsageAdminArgs, ReportUsageResult, ResetBudgetArgs, ResetBudgetResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, QuotaPolicyId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn create_budget_created_then_already_satisfied() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();

    let r1 = backend
        .create_budget(budget_args(&bid, 0, 100, 80))
        .await
        .expect("create_budget first call");
    assert!(matches!(r1, CreateBudgetResult::Created { .. }));

    let r2 = backend
        .create_budget(budget_args(&bid, 0, 100, 80))
        .await
        .expect("create_budget second call");
    assert!(matches!(r2, CreateBudgetResult::AlreadySatisfied { .. }));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn create_quota_policy_created_then_already_satisfied() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let qid = QuotaPolicyId::new();
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs(now_ms()),
    };

    let r1 = backend
        .create_quota_policy(args.clone())
        .await
        .expect("create_quota_policy first");
    assert!(matches!(r1, CreateQuotaPolicyResult::Created { .. }));

    let r2 = backend
        .create_quota_policy(args)
        .await
        .expect("create_quota_policy idempotent");
    assert!(matches!(r2, CreateQuotaPolicyResult::AlreadySatisfied { .. }));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn get_budget_status_happy_and_not_found() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();

    // Not-found → NotFound { entity: "budget" }.
    let missing = backend.get_budget_status(&bid).await.unwrap_err();
    match missing {
        EngineError::NotFound { entity } => assert_eq!(entity, "budget"),
        other => panic!("expected NotFound, got {other:?}"),
    }

    // Happy path after create_budget + report_usage_admin.
    backend
        .create_budget(budget_args(&bid, 0, 100, 80))
        .await
        .expect("create_budget");

    let status = backend.get_budget_status(&bid).await.expect("status");
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

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_admin_soft_then_hard_breach_updates_counters() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();

    backend
        .create_budget(budget_args(&bid, 0, /*hard*/ 100, /*soft*/ 50))
        .await
        .unwrap();

    // 60 tokens → soft breach (> 50, <= 100). Increment applied.
    let r1 = backend
        .report_usage_admin(
            &bid,
            ReportUsageAdminArgs::new(
                vec!["tokens".into()],
                vec![60],
                TimestampMs(now_ms()),
            ),
        )
        .await
        .unwrap();
    assert!(
        matches!(r1, ReportUsageResult::SoftBreach { .. }),
        "expected SoftBreach, got {r1:?}"
    );
    let status = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(status.soft_breach_count, 1);
    assert_eq!(status.breach_count, 0);
    assert_eq!(status.usage.get("tokens").copied(), Some(60));

    // Another 60 → would push to 120 > 100 hard limit. Hard breach,
    // no increment. `breach_count` increments; `last_breach_dim` set.
    let r2 = backend
        .report_usage_admin(
            &bid,
            ReportUsageAdminArgs::new(
                vec!["tokens".into()],
                vec![60],
                TimestampMs(now_ms()),
            ),
        )
        .await
        .unwrap();
    assert!(
        matches!(r2, ReportUsageResult::HardBreach { .. }),
        "expected HardBreach, got {r2:?}"
    );
    let status2 = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(status2.breach_count, 1);
    assert_eq!(status2.soft_breach_count, 1);
    assert_eq!(status2.last_breach_dim.as_deref(), Some("tokens"));
    assert_eq!(
        status2.usage.get("tokens").copied(),
        Some(60),
        "hard-breach must NOT apply the increment"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reset_budget_zeroes_usage_and_advances_next_reset() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();

    // reset_interval_ms = 3600s worth; create_budget seeds
    // next_reset_at_ms = now + interval.
    let interval_ms: u64 = 3_600_000;
    backend
        .create_budget(budget_args(&bid, interval_ms, 100, 80))
        .await
        .unwrap();
    backend
        .report_usage_admin(
            &bid,
            ReportUsageAdminArgs::new(
                vec!["tokens".into()],
                vec![40],
                TimestampMs(now_ms()),
            ),
        )
        .await
        .unwrap();

    let pre = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(pre.usage.get("tokens").copied(), Some(40));
    assert!(pre.next_reset_at.is_some());

    let now_before_reset = now_ms();
    let r = backend
        .reset_budget(ResetBudgetArgs {
            budget_id: bid.clone(),
            now: TimestampMs(now_before_reset),
        })
        .await
        .unwrap();
    let next = match r {
        ResetBudgetResult::Reset { next_reset_at } => next_reset_at.0,
        #[allow(unreachable_patterns)]
        _ => panic!("unexpected ResetBudgetResult shape"),
    };
    assert!(
        next >= now_before_reset + interval_ms as i64,
        "next_reset_at must advance: got {next}, expected >= {}",
        now_before_reset + interval_ms as i64,
    );

    let post = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(post.usage.get("tokens").copied(), Some(0));
    assert!(post.last_breach_at.is_none());
    assert!(post.last_breach_dim.is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reset_budget_not_found_returns_notfound() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();
    let err = backend
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

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn budget_reset_reconciler_zeroes_counters_and_advances() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool.clone());
    let partition_config = PartitionConfig::default();
    let bid = BudgetId::new();
    let interval_ms: u64 = 60_000; // 1 minute

    // Create with past `next_reset_at_ms` so the reconciler picks it up:
    // we bypass the now-plus-interval path on create_budget by directly
    // backdating the column to (now - 1).
    backend
        .create_budget(budget_args(&bid, interval_ms, 100, 80))
        .await
        .unwrap();
    backend
        .report_usage_admin(
            &bid,
            ReportUsageAdminArgs::new(
                vec!["tokens".into()],
                vec![25],
                TimestampMs(now_ms()),
            ),
        )
        .await
        .unwrap();

    // Backdate next_reset_at_ms so the reconciler sees it as due.
    let partition = ff_core::partition::budget_partition(&bid, &partition_config);
    let pk: i16 = partition.index as i16;
    let past = now_ms() - 1;
    sqlx::query(
        "UPDATE ff_budget_policy SET next_reset_at_ms = $3 \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(pk)
    .bind(bid.to_string())
    .bind(past)
    .execute(&pool)
    .await
    .unwrap();

    // Run one reconciler tick on the budget partition.
    let report =
        ff_backend_postgres::reconcilers::budget_reset::scan_tick(&pool, pk)
            .await
            .expect("scan_tick");
    assert!(report.processed >= 1, "reconciler must process >= 1 row");

    let post = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(post.usage.get("tokens").copied(), Some(0));
    // Scheduler must advance `next_reset_at_ms` to `now + interval`.
    let next_str = post.next_reset_at.expect("next_reset_at must be set");
    let next: i64 = next_str.parse().expect("parse next_reset_at as i64");
    assert!(next > past, "next_reset_at must advance past the backdated ts");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn report_usage_admin_dedup_key_is_idempotent() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = backend(pool);
    let bid = BudgetId::new();
    backend
        .create_budget(budget_args(&bid, 0, 100, 80))
        .await
        .unwrap();

    let dkey = format!("dedup-{}", uuid::Uuid::new_v4());
    let make = || {
        let mut a = ReportUsageAdminArgs::new(
            vec!["tokens".into()],
            vec![10],
            TimestampMs(now_ms()),
        );
        a.dedup_key = Some(dkey.clone());
        a
    };

    let r1 = backend.report_usage_admin(&bid, make()).await.unwrap();
    assert!(matches!(r1, ReportUsageResult::Ok));
    let r2 = backend.report_usage_admin(&bid, make()).await.unwrap();
    // Replay returns the cached outcome (Ok), not a double-counted
    // increment.
    assert!(
        matches!(r2, ReportUsageResult::Ok | ReportUsageResult::AlreadyApplied),
        "replay should be Ok or AlreadyApplied, got {r2:?}",
    );
    let status = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(
        status.usage.get("tokens").copied(),
        Some(10),
        "dedup'd replay must not double-count",
    );
}
