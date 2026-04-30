//! #453 / PR-7b: integration test for the typed-FCALL
//! `EngineBackend::renew_lease` body on Postgres.
//!
//! Covers the invariant set mirrored from `ff_renew_lease` in
//! `flowfabric.lua`:
//!
//! - happy path extends `ff_attempt.lease_expires_at_ms`
//! - missing fence → `Validation{InvalidInput, "fence_required"}`
//! - stale fence (wrong epoch) → `State(StaleLease)`
//! - expired lease → `State(LeaseExpired)`
//! - non-active lifecycle → `Contention(ExecutionNotActive { .. })`
//!
//! # Running
//!
//! `FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres \
//!   --test typed_renew_lease -- --ignored`

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{RenewLeaseArgs, RenewLeaseResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LeaseEpoch, LeaseFence, LeaseId, TimestampMs,
};
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
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

/// Seed an `ff_exec_core` row in `active` state plus a matching
/// `ff_attempt` row carrying a live lease (fence triple embedded in
/// `raw_fields`). Returns the triple the test will pass as `fence`.
async fn seed_active_lease(
    pool: &PgPool,
    lease_ttl_ms: i64,
) -> (i16, Uuid, ExecutionId, LeaseFence) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid =
        ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
    let lease_epoch: i64 = 1;
    let expires_at = now_ms() + lease_ttl_ms;

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
            $1, $2, 0,
            'w', 'wi',
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

async fn read_expires(pool: &PgPool, part: i16, exec_uuid: Uuid) -> i64 {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT lease_expires_at_ms FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = 0",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("fetch");
    row.try_get::<i64, _>("lease_expires_at_ms").unwrap()
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn renew_lease_extends_expires_at_on_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool, 5_000).await;
    let pre = read_expires(&pool, part, exec_uuid).await;

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    let res = backend.renew_lease(args).await.expect("renew ok");
    let RenewLeaseResult::Renewed { expires_at } = res;
    let post = read_expires(&pool, part, exec_uuid).await;
    assert!(
        post > pre,
        "lease_expires_at_ms should move forward (pre={pre}, post={post})"
    );
    assert_eq!(expires_at, TimestampMs::from_millis(post));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn renew_lease_rejects_missing_fence() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, _fence) = seed_active_lease(&pool, 5_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: None,
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match backend.renew_lease(args).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected Validation{{fence_required}}, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn renew_lease_stale_lease_on_wrong_epoch() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (_part, _exec_uuid, eid, fence) = seed_active_lease(&pool, 5_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let bad_fence = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 99),
        ..fence
    };
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(bad_fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match backend.renew_lease(args).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn renew_lease_expired_when_past_ttl() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    // Seed an already-expired lease by passing a negative TTL relative
    // to now. Simulates a lease that aged out before the worker
    // called renew.
    let (_part, _exec_uuid, eid, fence) = seed_active_lease(&pool, -5_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match backend.renew_lease(args).await {
        Err(EngineError::State(StateKind::LeaseExpired)) => {}
        other => panic!("expected LeaseExpired, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn renew_lease_execution_not_active_on_terminal() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let (part, exec_uuid, eid, fence) = seed_active_lease(&pool, 5_000).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .expect("flip to terminal");
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match backend.renew_lease(args).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
            lifecycle_phase,
            ..
        })) => {
            assert_eq!(lifecycle_phase, "terminal");
        }
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}
