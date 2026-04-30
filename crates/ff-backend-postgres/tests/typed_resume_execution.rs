//! #453 / PR-7b: integration test for `EngineBackend::resume_execution`
//! on Postgres.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{ResumeExecutionArgs, ResumeExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, StateKind};
use ff_core::partition::PartitionConfig;
use ff_core::state::PublicState;
use ff_core::types::ExecutionId;
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

async fn seed_suspended(pool: &PgPool) -> (i16, Uuid, ExecutionId) {
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
            'suspended', 'unowned', 'not_applicable',
            'waiting', 'attempt_suspended',
            0, $3
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec_core");
    sqlx::query(
        r#"
        INSERT INTO ff_suspension_current (
            partition_key, execution_id, suspension_id,
            suspended_at_ms, timeout_at_ms, reason_code,
            condition
        ) VALUES ($1, $2, $3, $4, NULL, 'await_signal', '{}'::jsonb)
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(Uuid::new_v4())
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed suspension_current");
    (part, exec_uuid, eid)
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn resume_happy_path_no_delay() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_suspended(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match backend.resume_execution(args).await.expect("resume ok") {
        ResumeExecutionResult::Resumed { public_state } => {
            assert_eq!(public_state, PublicState::Waiting);
        }
    }
    let row = sqlx::query(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "runnable");
    assert_eq!(row.get::<String, _>("public_state"), "waiting");
    let (cnt,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_suspension_current \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cnt, 0, "suspension_current row must be deleted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn resume_with_delay_becomes_delayed() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_suspended(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "operator".into(),
        resume_delay_ms: 5_000,
    };
    match backend.resume_execution(args).await.expect("ok") {
        ResumeExecutionResult::Resumed { public_state } => {
            assert_eq!(public_state, PublicState::Delayed);
        }
    }
    let row = sqlx::query(
        "SELECT public_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("public_state"), "delayed");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn resume_rejects_non_suspended() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_suspended(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='active' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = ResumeExecutionArgs {
        execution_id: eid,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match backend.resume_execution(args).await {
        Err(EngineError::State(StateKind::ExecutionNotSuspended)) => {}
        other => panic!("expected ExecutionNotSuspended, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn resume_not_found_without_exec_core() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let bogus =
        ExecutionId::parse("{fp:0}:99999999-9999-9999-9999-999999999999").unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool, PartitionConfig::default());
    let args = ResumeExecutionArgs {
        execution_id: bogus,
        trigger_type: "signal".into(),
        resume_delay_ms: 0,
    };
    match backend.resume_execution(args).await {
        Err(EngineError::NotFound { entity }) => assert_eq!(entity, "execution"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}
