//! #453 / PR-7b: integration test for `EngineBackend::fail_execution`
//! on Postgres.
//!
//! Covers both exit branches (retry scheduled + terminal failed) plus
//! the error matrix shared with `complete_execution`.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{FailExecutionArgs, FailExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, LeaseEpoch, LeaseFence, LeaseId,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
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
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

async fn seed_active_lease(pool: &PgPool) -> (i16, Uuid, ExecutionId, LeaseFence) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
    let lease_epoch: i64 = 1;

    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms, raw_fields
        ) VALUES (
            $1, $2, NULL, 'default',
            '{}', 0,
            'active', 'leased', 'not_applicable',
            'active', 'running_attempt',
            0, $3,
            jsonb_build_object('current_attempt_id', $4::text)
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .bind(attempt_id.to_string())
    .execute(pool)
    .await
    .expect("insert exec");
    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms
        ) VALUES ($1, $2, 0, 'w', 'wi', $3, $4, $5)
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lease_epoch)
    .bind(now_ms() + 60_000)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("insert attempt");
    let fence = LeaseFence {
        lease_id,
        lease_epoch: LeaseEpoch(lease_epoch as u64),
        attempt_id,
    };
    (part, exec_uuid, eid, fence)
}

async fn read_phase_and_public_state(pool: &PgPool, part: i16, exec_uuid: Uuid) -> (String, String) {
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("fetch");
    (
        row.try_get::<String, _>("lifecycle_phase").unwrap(),
        row.try_get::<String, _>("public_state").unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn fail_terminal_when_no_retries() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = FailExecutionArgs {
        execution_id: eid,
        fence: Some(fence),
        attempt_index: AttemptIndex::new(0),
        failure_reason: "boom".into(),
        failure_category: "transient".into(),
        retry_policy_json: String::new(), // no retries
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    };
    let res = backend.fail_execution(args).await.expect("fail ok");
    assert_eq!(res, FailExecutionResult::TerminalFailed);
    let (phase, pub_state) = read_phase_and_public_state(&pool, part, exec_uuid).await;
    assert_eq!(phase, "terminal");
    assert_eq!(pub_state, "failed");
    let (evt_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_completion_event \
         WHERE partition_key = $1 AND execution_id = $2 AND outcome = 'failed'",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(evt_count, 1);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn fail_schedules_retry_when_budget_remains() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    // Policy: max 3 retries, fixed 500 ms delay.
    let policy = r#"{"max_retries":3,"backoff":{"type":"fixed","delay_ms":500}}"#;
    let args = FailExecutionArgs {
        execution_id: eid,
        fence: Some(fence),
        attempt_index: AttemptIndex::new(0),
        failure_reason: "transient".into(),
        failure_category: "network".into(),
        retry_policy_json: policy.into(),
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    };
    let res = backend.fail_execution(args).await.expect("fail ok");
    match res {
        FailExecutionResult::RetryScheduled {
            next_attempt_index, ..
        } => {
            assert_eq!(next_attempt_index, AttemptIndex::new(1));
        }
        other => panic!("expected RetryScheduled, got {other:?}"),
    }
    let (phase, pub_state) = read_phase_and_public_state(&pool, part, exec_uuid).await;
    assert_eq!(phase, "runnable");
    assert_eq!(pub_state, "delayed");
    // raw_fields.retry_count bumped to "1".
    let (rc,): (String,) = sqlx::query_as(
        "SELECT raw_fields->>'retry_count' FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .expect("fetch");
    assert_eq!(rc, "1");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn fail_rejects_missing_fence_without_override() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, _fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = FailExecutionArgs {
        execution_id: eid,
        fence: None,
        attempt_index: AttemptIndex::new(0),
        failure_reason: "x".into(),
        failure_category: "y".into(),
        retry_policy_json: String::new(),
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    };
    match backend.fail_execution(args).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected fence_required, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn fail_stale_lease_on_wrong_epoch() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let bad = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 9),
        ..fence
    };
    let args = FailExecutionArgs {
        execution_id: eid,
        fence: Some(bad),
        attempt_index: AttemptIndex::new(0),
        failure_reason: "x".into(),
        failure_category: "y".into(),
        retry_policy_json: String::new(),
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    };
    match backend.fail_execution(args).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn fail_execution_not_active_on_terminal() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .expect("flip");
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = FailExecutionArgs {
        execution_id: eid,
        fence: Some(fence),
        attempt_index: AttemptIndex::new(0),
        failure_reason: "x".into(),
        failure_category: "y".into(),
        retry_policy_json: String::new(),
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    };
    match backend.fail_execution(args).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}
