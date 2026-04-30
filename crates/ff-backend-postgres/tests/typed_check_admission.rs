//! #453 / PR-7b: integration test for `EngineBackend::check_admission`
//! on Postgres.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{CheckAdmissionArgs, CheckAdmissionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{ExecutionId, QuotaPolicyId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
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
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool).await.expect("migrate");
    Some(pool)
}

fn fresh_eid() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).unwrap()
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn admits_first_request_in_window() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    let args = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 10,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    let res = backend
        .check_admission(&qid, "", args)
        .await
        .expect("admit");
    assert_eq!(res, CheckAdmissionResult::Admitted);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn idempotent_second_admit_returns_already_admitted() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    let eid = fresh_eid();
    let base = CheckAdmissionArgs {
        execution_id: eid.clone(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 10,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    assert_eq!(
        backend.check_admission(&qid, "", base.clone()).await.unwrap(),
        CheckAdmissionResult::Admitted
    );
    assert_eq!(
        backend.check_admission(&qid, "", base).await.unwrap(),
        CheckAdmissionResult::AlreadyAdmitted
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn rate_exceeded_returns_retry_after() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    // Admit 3 different executions; 4th with rate_limit=3 should be
    // RateExceeded.
    for _ in 0..3 {
        let args = CheckAdmissionArgs {
            execution_id: fresh_eid(),
            now: TimestampMs::from_millis(now_ms()),
            window_seconds: 60,
            rate_limit: 3,
            concurrency_cap: 100,
            jitter_ms: None,
        };
        assert_eq!(
            backend.check_admission(&qid, "", args).await.unwrap(),
            CheckAdmissionResult::Admitted
        );
    }
    let fourth = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 3,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    match backend.check_admission(&qid, "", fourth).await.unwrap() {
        CheckAdmissionResult::RateExceeded { retry_after_ms } => {
            assert!(retry_after_ms > 0 || retry_after_ms == 0);
        }
        other => panic!("expected RateExceeded, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn concurrency_cap_blocks_additional_admits() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    // Pre-load the policy row with active_concurrency=2, cap=2 so the
    // next admit hits the cap before inserting.
    let part = ff_core::partition::quota_partition(&qid, &PartitionConfig::default()).index as i16;
    sqlx::query(
        "INSERT INTO ff_quota_policy \
            (partition_key, quota_policy_id, \
             requests_per_window_seconds, max_requests_per_window, \
             active_concurrency_cap, active_concurrency, \
             created_at_ms, updated_at_ms) \
         VALUES ($1, $2, 60, 100, 2, 2, $3, $3)",
    )
    .bind(part)
    .bind(qid.to_string())
    .bind(now_ms())
    .execute(&pool)
    .await
    .expect("seed policy");
    let args = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 100,
        concurrency_cap: 2,
        jitter_ms: None,
    };
    match backend.check_admission(&qid, "", args).await.unwrap() {
        CheckAdmissionResult::ConcurrencyExceeded => {}
        other => panic!("expected ConcurrencyExceeded, got {other:?}"),
    }
}
