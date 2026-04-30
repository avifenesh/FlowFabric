//! Phase 1 integration test for SQLite claim_execution.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{ClaimExecutionArgs, ClaimExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseId, TimestampMs, WorkerId, WorkerInstanceId,
};
use serial_test::serial;
use sqlx::Row;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

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
    let uri = format!("file:typed-claim-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct")
}

async fn seed_runnable(backend: &SqliteBackend) -> (Uuid, ExecutionId) {
    let pool = backend.pool_for_test();
    let part: i64 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state, priority, created_at_ms, raw_fields
        ) VALUES (
            ?1, ?2, NULL, 'default', 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt', 0, ?3,
            '{}'
        )
        "#,
    )
    .bind(part).bind(exec_uuid).bind(now_ms())
    .execute(pool).await.expect("seed exec");
    (exec_uuid, eid)
}

async fn seed_grant(backend: &SqliteBackend, exec_uuid: Uuid, worker_id: &str, expires_at_ms: i64) {
    let pool = backend.pool_for_test();
    let grant_id_bytes = Uuid::new_v4().as_bytes().to_vec();
    sqlx::query(
        "INSERT INTO ff_claim_grant (
            partition_key, grant_id, execution_id, kind,
            worker_id, worker_instance_id, lane_id,
            capability_hash, worker_capabilities, route_snapshot_json,
            admission_summary, grant_ttl_ms, issued_at_ms, expires_at_ms
         ) VALUES (0, ?1, ?2, 'claim', ?3, 'wi', 'default',
                   NULL, '[]', NULL, NULL,
                   60000, ?4, ?5)",
    )
    .bind(grant_id_bytes).bind(exec_uuid).bind(worker_id).bind(now_ms()).bind(expires_at_ms)
    .execute(pool).await.expect("seed grant");
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
#[serial(ff_dev_mode)]
async fn claim_happy_path() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&backend).await;
    seed_grant(&backend, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let res = be.claim_execution(fresh_args(eid)).await.expect("claim ok");
    match res {
        ClaimExecutionResult::Claimed(_) => {}
        #[allow(unreachable_patterns)]
        other => panic!("expected Claimed, got {other:?}"),
    }
    let row = sqlx::query(
        "SELECT lifecycle_phase, ownership_state FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid).fetch_one(backend.pool_for_test()).await.unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "active");
    assert_eq!(row.get::<String, _>("ownership_state"), "leased");
    let (cnt,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant \
         WHERE partition_key = 0 AND execution_id = ?1 AND kind='claim'",
    )
    .bind(exec_uuid).fetch_one(backend.pool_for_test()).await.unwrap();
    assert_eq!(cnt, 0, "grant deleted");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_rejects_expired_grant() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&backend).await;
    seed_grant(&backend, exec_uuid, "worker-1", now_ms() - 1_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::ClaimGrantExpired)) => {}
        other => panic!("expected ClaimGrantExpired, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_rejects_wrong_worker_grant() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&backend).await;
    seed_grant(&backend, exec_uuid, "someone-else", now_ms() + 60_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::InvalidClaimGrant)) => {}
        other => panic!("expected InvalidClaimGrant, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_rejects_missing_grant() {
    let backend = fresh_backend().await;
    let (_u, eid) = seed_runnable(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::InvalidClaimGrant)) => {}
        other => panic!("expected InvalidClaimGrant, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_rejects_non_runnable() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid).execute(backend.pool_for_test()).await.unwrap();
    seed_grant(&backend, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_rejects_already_owned() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='leased' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid).execute(backend.pool_for_test()).await.unwrap();
    seed_grant(&backend, exec_uuid, "worker-1", now_ms() + 60_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.claim_execution(fresh_args(eid)).await {
        Err(EngineError::Contention(ContentionKind::LeaseConflict)) => {}
        other => panic!("expected LeaseConflict, got {other:?}"),
    }
}
