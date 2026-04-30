//! #33 / Phase 1: integration test for SQLite `EngineBackend::fail_execution`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{FailExecutionArgs, FailExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, LeaseEpoch, LeaseFence, LeaseId,
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
    let uri = format!("file:typed-fail-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct")
}

async fn seed_active(backend: &SqliteBackend) -> (Uuid, ExecutionId, LeaseFence) {
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
            partition_key, execution_id, flow_id, lane_id, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state, priority, created_at_ms, raw_fields
        ) VALUES (
            ?1, ?2, NULL, 'default', 0,
            'active', 'leased', 'not_applicable',
            'active', 'running_attempt', 0, ?3,
            json_set('{}', '$.current_attempt_id', ?4)
        )
        "#,
    )
    .bind(part).bind(exec_uuid).bind(now_ms()).bind(attempt_id.to_string())
    .execute(pool).await.expect("seed exec");
    sqlx::query(
        "INSERT INTO ff_attempt (partition_key, execution_id, attempt_index, \
                                  worker_id, worker_instance_id, \
                                  lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES (?1, ?2, 0, 'w', 'wi', ?3, ?4, ?5)",
    )
    .bind(part).bind(exec_uuid).bind(lease_epoch).bind(now_ms() + 60_000).bind(now_ms())
    .execute(pool).await.expect("seed attempt");
    let fence = LeaseFence {
        lease_id,
        lease_epoch: LeaseEpoch(lease_epoch as u64),
        attempt_id,
    };
    (exec_uuid, eid, fence)
}

fn base_args(eid: ExecutionId, fence: Option<LeaseFence>, retry_policy: &str) -> FailExecutionArgs {
    FailExecutionArgs {
        execution_id: eid,
        fence,
        attempt_index: AttemptIndex::new(0),
        failure_reason: "boom".into(),
        failure_category: "transient".into(),
        retry_policy_json: retry_policy.into(),
        next_attempt_policy_json: String::new(),
        source: CancelSource::LeaseHolder,
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_terminal_when_no_retries() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let res = be
        .fail_execution(base_args(eid, Some(fence), ""))
        .await
        .expect("fail ok");
    assert_eq!(res, FailExecutionResult::TerminalFailed);
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "terminal");
    assert_eq!(row.get::<String, _>("public_state"), "failed");
    let (c,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_completion_event \
         WHERE partition_key = 0 AND execution_id = ?1 AND outcome = 'failed'",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(c, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_schedules_retry_when_budget_remains() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let policy = r#"{"max_retries":3,"backoff":{"type":"fixed","delay_ms":500}}"#;
    let res = be
        .fail_execution(base_args(eid, Some(fence), policy))
        .await
        .expect("fail ok");
    match res {
        FailExecutionResult::RetryScheduled {
            next_attempt_index, ..
        } => assert_eq!(next_attempt_index, AttemptIndex::new(1)),
        other => panic!("expected RetryScheduled, got {other:?}"),
    }
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state, \
                json_extract(raw_fields, '$.retry_count') AS retry_count \
         FROM ff_exec_core WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "runnable");
    assert_eq!(row.get::<String, _>("public_state"), "delayed");
    assert_eq!(row.get::<String, _>("retry_count"), "1");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_rejects_missing_fence_without_override() {
    let backend = fresh_backend().await;
    let (_u, eid, _f) = seed_active(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.fail_execution(base_args(eid, None, "")).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected fence_required, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_stale_lease_on_wrong_epoch() {
    let backend = fresh_backend().await;
    let (_u, eid, fence) = seed_active(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let bad = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 9),
        ..fence
    };
    match be.fail_execution(base_args(eid, Some(bad), "")).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_execution_not_active_on_terminal() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .unwrap();
    let be: Arc<dyn EngineBackend> = backend.clone();
    match be.fail_execution(base_args(eid, Some(fence), "")).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}
