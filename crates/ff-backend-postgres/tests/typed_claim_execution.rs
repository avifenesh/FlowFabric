//! #453 / PR-7b: integration test for EngineBackend::claim_execution
//! on Postgres.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{ClaimExecutionArgs, ClaimExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseId, TimestampMs, WorkerId,
    WorkerInstanceId,
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
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool).await.expect("migrate");
    Some(pool)
}

async fn seed_runnable(pool: &PgPool) -> (i16, Uuid, ExecutionId) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms
        ) VALUES (
            $1, $2, NULL, 'default', '{}', 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $3
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec");
    (part, exec_uuid, eid)
}

async fn seed_grant(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    worker_id: &str,
    expires_at_ms: i64,
) {
    let grant_id_bytes: Vec<u8> = Uuid::new_v4().as_bytes().to_vec();
    sqlx::query(
        "INSERT INTO ff_claim_grant (
            partition_key, grant_id, execution_id, kind,
            worker_id, worker_instance_id, lane_id,
            capability_hash, worker_capabilities,
            route_snapshot_json, admission_summary,
            grant_ttl_ms, issued_at_ms, expires_at_ms
         ) VALUES (
            $1, $2, $3, 'claim',
            $4, 'wi', 'default',
            NULL, '[]'::jsonb, NULL, NULL,
            60000, $5, $6
         )",
    )
    .bind(part)
    .bind(grant_id_bytes)
    .bind(exec_uuid)
    .bind(worker_id)
    .bind(now_ms())
    .bind(expires_at_ms)
    .execute(pool)
    .await
    .expect("seed grant");
}

fn fresh_args(eid: ExecutionId) -> ClaimExecutionArgs {
    ClaimExecutionArgs::new(
        eid,
        WorkerId::new("worker-1"),
        WorkerInstanceId::new("wi-1"),
        LaneId::new("default"),
        LeaseId::new(),
        60_000,
        AttemptId::new(),
        AttemptIndex::new(0),
        String::new(),
        None,
        None,
        TimestampMs::from_millis(now_ms()),
    )
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    seed_grant(&pool, part, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = fresh_args(eid);
    let res = backend.claim_execution(args).await.expect("claim ok");
    match res {
        ClaimExecutionResult::Claimed(_) => {}
        #[allow(unreachable_patterns)]
        other => panic!("expected Claimed, got {other:?}"),
    }
    let row = sqlx::query(
        "SELECT lifecycle_phase, ownership_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "active");
    assert_eq!(row.get::<String, _>("ownership_state"), "leased");
    let (gcnt,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant \
         WHERE partition_key = $1 AND execution_id = $2 AND kind='claim'",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(gcnt, 0, "claim grant must be deleted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_rejects_expired_grant() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    seed_grant(&pool, part, exec_uuid, "worker-1", now_ms() - 1_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::ClaimGrantExpired)) => {}
        other => panic!("expected ClaimGrantExpired, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_rejects_wrong_worker_grant() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    seed_grant(&pool, part, exec_uuid, "someone-else", now_ms() + 60_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::InvalidClaimGrant)) => {}
        other => panic!("expected InvalidClaimGrant, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_rejects_missing_grant() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (_part, _exec_uuid, eid) = seed_runnable(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::InvalidClaimGrant)) => {}
        other => panic!("expected InvalidClaimGrant, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_rejects_non_runnable_exec() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    seed_grant(&pool, part, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn claim_rejects_already_owned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='leased' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    seed_grant(&pool, part, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::LeaseConflict)) => {}
        other => panic!("expected LeaseConflict, got {other:?}"),
    }
}
