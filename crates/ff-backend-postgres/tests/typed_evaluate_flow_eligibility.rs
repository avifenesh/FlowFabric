//! #453 / PR-7b: integration test for
//! EngineBackend::evaluate_flow_eligibility on Postgres.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{EvaluateFlowEligibilityArgs, EvaluateFlowEligibilityResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::ExecutionId;
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

async fn seed_exec(
    pool: &PgPool,
    lifecycle_phase: &str,
    ownership: &str,
    flow_id: Option<Uuid>,
    terminal_outcome: &str,
) -> (i16, Uuid, ExecutionId) {
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
            priority, created_at_ms, raw_fields
        ) VALUES (
            $1, $2, $3, 'default', '{}', 0,
            $4, $5, 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $6,
            jsonb_build_object('terminal_outcome', $7::text)
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(flow_id)
    .bind(lifecycle_phase)
    .bind(ownership)
    .bind(now_ms())
    .bind(terminal_outcome)
    .execute(pool)
    .await
    .expect("seed");
    (part, exec_uuid, eid)
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn eligible_when_standalone_runnable_unowned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (_p, _u, eid) = seed_exec(&pool, "runnable", "unowned", None, "none").await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await
        .expect("ok");
    assert_eq!(status, "eligible");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn not_found_without_row() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let bogus =
        ExecutionId::parse("{fp:0}:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: bogus })
        .await
        .expect("ok");
    assert_eq!(status, "not_found");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn not_runnable_when_active() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (_p, _u, eid) = seed_exec(&pool, "active", "leased", None, "none").await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await
        .expect("ok");
    assert_eq!(status, "not_runnable");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn owned_when_ownership_not_unowned() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (_p, _u, eid) = seed_exec(&pool, "runnable", "leased", None, "none").await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await
        .expect("ok");
    assert_eq!(status, "owned");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn blocked_by_required_edge_with_non_terminal_upstream() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core \
            (partition_key, flow_id, public_flow_state, created_at_ms) \
         VALUES (0, $1, 'running', $2)",
    )
    .bind(flow_id)
    .bind(now_ms())
    .execute(&pool)
    .await
    .expect("seed flow");

    let (_p, up_uuid, _up_eid) =
        seed_exec(&pool, "runnable", "unowned", Some(flow_id), "none").await;
    let (_p, down_uuid, down_eid) =
        seed_exec(&pool, "runnable", "unowned", Some(flow_id), "none").await;

    sqlx::query(
        "INSERT INTO ff_edge \
            (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES (0, $1, $2, $3, $4, '{\"kind\":\"required\"}'::jsonb)",
    )
    .bind(flow_id)
    .bind(Uuid::new_v4())
    .bind(up_uuid)
    .bind(down_uuid)
    .execute(&pool)
    .await
    .expect("seed edge");

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs {
            execution_id: down_eid,
        })
        .await
        .expect("ok");
    assert_eq!(status, "blocked_by_dependencies");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn impossible_when_required_upstream_failed() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core \
            (partition_key, flow_id, public_flow_state, created_at_ms) \
         VALUES (0, $1, 'running', $2)",
    )
    .bind(flow_id)
    .bind(now_ms())
    .execute(&pool)
    .await
    .expect("seed flow");

    let (_p, up_uuid, _up_eid) =
        seed_exec(&pool, "terminal", "unowned", Some(flow_id), "failed").await;
    let (_p, down_uuid, down_eid) =
        seed_exec(&pool, "runnable", "unowned", Some(flow_id), "none").await;

    sqlx::query(
        "INSERT INTO ff_edge \
            (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES (0, $1, $2, $3, $4, '{\"kind\":\"required\"}'::jsonb)",
    )
    .bind(flow_id)
    .bind(Uuid::new_v4())
    .bind(up_uuid)
    .bind(down_uuid)
    .execute(&pool)
    .await
    .expect("seed edge");

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let EvaluateFlowEligibilityResult::Status { status } = backend
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs {
            execution_id: down_eid,
        })
        .await
        .expect("ok");
    assert_eq!(status, "impossible");
}
