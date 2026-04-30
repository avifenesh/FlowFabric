//! #33 / Phase 1: integration test for SQLite
//! `EngineBackend::resume_execution`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{ResumeExecutionArgs, ResumeExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, StateKind};
use ff_core::state::PublicState;
use ff_core::types::ExecutionId;
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
    let uri = format!("file:typed-resume-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct")
}

async fn seed_suspended(backend: &SqliteBackend) -> (Uuid, ExecutionId) {
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
            'suspended', 'unowned', 'not_applicable',
            'waiting', 'attempt_suspended', 0, ?3,
            '{}'
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec");
    sqlx::query(
        "INSERT INTO ff_suspension_current (
            partition_key, execution_id, suspension_id, suspended_at_ms,
            timeout_at_ms, reason_code, condition
         ) VALUES (?1, ?2, ?3, ?4, NULL, 'await_signal', '{}')",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(Uuid::new_v4())
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed suspension");
    (exec_uuid, eid)
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn resume_happy_path_no_delay() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_suspended(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match be.resume_execution(args).await.expect("resume ok") {
        ResumeExecutionResult::Resumed { public_state } => {
            assert_eq!(public_state, PublicState::Waiting);
        }
    }
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "runnable");
    assert_eq!(row.get::<String, _>("public_state"), "waiting");
    let (cnt,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_suspension_current \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(cnt, 0, "suspension_current row deleted");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn resume_with_delay_becomes_delayed() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_suspended(&backend).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "operator".into(),
        resume_delay_ms: 5_000,
    };
    match be.resume_execution(args).await.expect("ok") {
        ResumeExecutionResult::Resumed { public_state } => {
            assert_eq!(public_state, PublicState::Delayed);
        }
    }
    let row = sqlx::query(
        "SELECT public_state FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("public_state"), "delayed");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn resume_rejects_non_suspended() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid) = seed_suspended(&backend).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='active' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .unwrap();
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match be.resume_execution(args).await {
        Err(EngineError::State(StateKind::ExecutionNotSuspended)) => {}
        other => panic!("expected ExecutionNotSuspended, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn resume_not_found_without_exec_core() {
    let backend = fresh_backend().await;
    let bogus =
        ExecutionId::parse("{fp:0}:99999999-9999-9999-9999-999999999999").unwrap();
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = ResumeExecutionArgs {
        execution_id: bogus,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match be.resume_execution(args).await {
        Err(EngineError::NotFound { entity }) => assert_eq!(entity, "execution"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}
