//! #453 / PR-7b: integration test for the typed-FCALL
//! `EngineBackend::complete_execution` body on Postgres.
//!
//! Covers the invariant set mirrored from `ff_complete_execution` in
//! `flowfabric.lua`:
//!
//! - happy path flips lifecycle_phase=terminal, public_state=completed,
//!   stores result payload, marks attempt terminal, emits completion
//!   outbox event
//! - fence_required → `Validation{InvalidInput}` when fence=None and
//!   source != OperatorOverride
//! - operator override path: fence=None + OperatorOverride bypasses
//!   epoch check but still honours lifecycle gate
//! - stale fence → `State(StaleLease)`
//! - non-active lifecycle → `Contention(ExecutionNotActive{...})`
//!
//! # Running
//!
//! `FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres \
//!   --test typed_complete_execution -- --ignored`

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{CompleteExecutionArgs, CompleteExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, LeaseEpoch, LeaseFence, LeaseId,
    TimestampMs,
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

/// Seed an `active`+leased execution ready to be completed.
async fn seed_active_lease(
    pool: &PgPool,
) -> (i16, Uuid, ExecutionId, LeaseFence) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid =
        ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
    let lease_epoch: i64 = 1;
    let expires_at = now_ms() + 60_000;

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
    .expect("insert ff_exec_core");

    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms
        ) VALUES (
            $1, $2, 0, 'w', 'wi',
            $3, $4, $5
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lease_epoch)
    .bind(expires_at)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("insert ff_attempt");

    let fence = LeaseFence {
        lease_id,
        lease_epoch: LeaseEpoch(lease_epoch as u64),
        attempt_id,
    };
    (part, exec_uuid, eid, fence)
}

async fn read_exec_state(pool: &PgPool, part: i16, exec_uuid: Uuid) -> (String, String) {
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("fetch exec_core");
    (
        row.try_get::<String, _>("lifecycle_phase").unwrap(),
        row.try_get::<String, _>("public_state").unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn complete_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool).await;

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = CompleteExecutionArgs {
        execution_id: eid.clone(),
        fence: Some(fence),
        attempt_index: AttemptIndex::new(0),
        result_payload: Some(b"ok".to_vec()),
        result_encoding: Some("text/plain".into()),
        source: CancelSource::LeaseHolder,
        now: TimestampMs::from_millis(now_ms()),
    };
    let res = backend.complete_execution(args).await.expect("complete ok");
    match res {
        CompleteExecutionResult::Completed {
            execution_id,
            public_state,
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(public_state, PublicState::Completed);
        }
    }

    let (phase, pub_state) = read_exec_state(&pool, part, exec_uuid).await;
    assert_eq!(phase, "terminal");
    assert_eq!(pub_state, "completed");

    // Completion outbox fired.
    let (evt_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_completion_event \
         WHERE partition_key = $1 AND execution_id = $2 AND outcome = 'success'",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .expect("count outbox");
    assert_eq!(evt_count, 1, "completion outbox event must be emitted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn complete_rejects_missing_fence_without_override() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, _fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = CompleteExecutionArgs {
        execution_id: eid,
        fence: None,
        attempt_index: AttemptIndex::new(0),
        result_payload: None,
        result_encoding: None,
        source: CancelSource::LeaseHolder,
        now: TimestampMs::from_millis(now_ms()),
    };
    match backend.complete_execution(args).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected Validation{{fence_required}}, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn complete_operator_override_skips_fence() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, _fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = CompleteExecutionArgs {
        execution_id: eid,
        fence: None,
        attempt_index: AttemptIndex::new(0),
        result_payload: None,
        result_encoding: None,
        source: CancelSource::OperatorOverride,
        now: TimestampMs::from_millis(now_ms()),
    };
    backend.complete_execution(args).await.expect("override ok");
    let (phase, _) = read_exec_state(&pool, part, exec_uuid).await;
    assert_eq!(phase, "terminal");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn complete_stale_lease_on_wrong_epoch() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let bad_fence = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 7),
        ..fence
    };
    let args = CompleteExecutionArgs {
        execution_id: eid,
        fence: Some(bad_fence),
        attempt_index: AttemptIndex::new(0),
        result_payload: None,
        result_encoding: None,
        source: CancelSource::LeaseHolder,
        now: TimestampMs::from_millis(now_ms()),
    };
    match backend.complete_execution(args).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn complete_execution_not_active_on_terminal() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .expect("flip");
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = CompleteExecutionArgs {
        execution_id: eid,
        fence: Some(fence),
        attempt_index: AttemptIndex::new(0),
        result_payload: None,
        result_encoding: None,
        source: CancelSource::LeaseHolder,
        now: TimestampMs::from_millis(now_ms()),
    };
    match backend.complete_execution(args).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
            lifecycle_phase,
            ..
        })) => {
            assert_eq!(lifecycle_phase, "terminal");
        }
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}
