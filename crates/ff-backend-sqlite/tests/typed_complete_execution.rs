//! #33 / Phase 1: integration test for SQLite
//! `EngineBackend::complete_execution`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{CompleteExecutionArgs, CompleteExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, LeaseEpoch, LeaseFence, LeaseId,
    TimestampMs,
};
use serial_test::serial;
use sqlx::Row;
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
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:typed-complete-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri)
        .await
        .expect("construct with migrations")
}

async fn seed_active_lease(
    backend: &SqliteBackend,
) -> (Uuid, ExecutionId, LeaseFence) {
    let pool = backend.pool_for_test();
    let part: i64 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
    let lease_epoch: i64 = 1;

    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms, raw_fields
        ) VALUES (
            ?1, ?2, NULL, 'default',
            0,
            'active', 'leased', 'not_applicable',
            'active', 'running_attempt',
            0, ?3,
            json_set('{}', '$.current_attempt_id', ?4)
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .bind(attempt_id.to_string())
    .execute(pool)
    .await
    .expect("insert exec_core");
    sqlx::query(
        "INSERT INTO ff_attempt (partition_key, execution_id, attempt_index, \
                                  worker_id, worker_instance_id, \
                                  lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES (?1, ?2, 0, 'w', 'wi', ?3, ?4, ?5)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lease_epoch)
    .bind(now_ms() + 60_000)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("insert ff_attempt");
    let fence = LeaseFence {
        lease_id,
        lease_epoch: LeaseEpoch(lease_epoch as u64),
        attempt_id,
    };
    (exec_uuid, eid, fence)
}

async fn read_phase_and_public_state(
    backend: &SqliteBackend,
    exec_uuid: Uuid,
) -> (String, String) {
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .expect("fetch");
    (
        row.try_get::<String, _>("lifecycle_phase").unwrap(),
        row.try_get::<String, _>("public_state").unwrap(),
    )
}

fn base_args(eid: ExecutionId, fence: Option<LeaseFence>) -> CompleteExecutionArgs {
    CompleteExecutionArgs {
        execution_id: eid,
        fence,
        attempt_index: AttemptIndex::new(0),
        result_payload: None,
        result_encoding: None,
        source: CancelSource::LeaseHolder,
        now: TimestampMs::from_millis(now_ms()),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_happy_path() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let mut args = base_args(eid.clone(), Some(fence));
    args.result_payload = Some(b"ok".to_vec());
    let res = be.complete_execution(args).await.expect("complete ok");
    match res {
        CompleteExecutionResult::Completed {
            execution_id,
            public_state,
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(public_state, PublicState::Completed);
        }
    }
    let (phase, ps) = read_phase_and_public_state(&backend, exec_uuid).await;
    assert_eq!(phase, "terminal");
    assert_eq!(ps, "completed");
    let (cnt,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_completion_event \
         WHERE partition_key = 0 AND execution_id = ?1 AND outcome = 'success'",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(cnt, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_rejects_missing_fence_without_override() {
    let backend = fresh_backend().await;
    let (_uuid, eid, _fence) = seed_active_lease(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = base_args(eid, None);
    match be.complete_execution(args).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected Validation{{fence_required}}, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_operator_override_skips_fence() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, _fence) = seed_active_lease(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let mut args = base_args(eid, None);
    args.source = CancelSource::OperatorOverride;
    be.complete_execution(args).await.expect("override ok");
    let (phase, _) = read_phase_and_public_state(&backend, exec_uuid).await;
    assert_eq!(phase, "terminal");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_stale_lease_on_wrong_epoch() {
    let backend = fresh_backend().await;
    let (_uuid, eid, fence) = seed_active_lease(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let bad = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 7),
        ..fence
    };
    let args = base_args(eid, Some(bad));
    match be.complete_execution(args).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_execution_not_active_on_terminal() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .expect("flip");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = base_args(eid, Some(fence));
    match be.complete_execution(args).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
            lifecycle_phase,
            ..
        })) => {
            assert_eq!(lifecycle_phase, "terminal");
        }
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_lease_revoked_on_ownership_revoked() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state = 'lease_revoked' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .expect("flip");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = base_args(eid, Some(fence));
    match be.complete_execution(args).await {
        Err(EngineError::State(StateKind::LeaseRevoked)) => {}
        other => panic!("expected LeaseRevoked, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_lease_expired_on_past_ttl() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend).await;
    // Backdate attempt's lease to already-expired.
    sqlx::query(
        "UPDATE ff_attempt SET lease_expires_at_ms = ?1 \
         WHERE partition_key = 0 AND execution_id = ?2 AND attempt_index = 0",
    )
    .bind(now_ms() - 5_000)
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .expect("backdate");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = base_args(eid, Some(fence));
    match be.complete_execution(args).await {
        Err(EngineError::State(StateKind::LeaseExpired)) => {}
        other => panic!("expected LeaseExpired, got {other:?}"),
    }
}
