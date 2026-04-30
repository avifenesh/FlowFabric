//! Phase 1 integration test for SQLite evaluate_flow_eligibility.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{EvaluateFlowEligibilityArgs, EvaluateFlowEligibilityResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::ExecutionId;
use serial_test::serial;
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
    let uri = format!("file:typed-eval-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct")
}

async fn seed_exec(
    backend: &SqliteBackend,
    lifecycle_phase: &str,
    ownership: &str,
    flow_id: Option<Uuid>,
    terminal_outcome: &str,
) -> (Uuid, ExecutionId) {
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
            ?1, ?2, ?3, 'default', 0,
            ?4, ?5, 'eligible_now',
            'waiting', 'pending_first_attempt', 0, ?6,
            json_set('{}', '$.terminal_outcome', ?7)
        )
        "#,
    )
    .bind(part).bind(exec_uuid).bind(flow_id).bind(lifecycle_phase).bind(ownership).bind(now_ms()).bind(terminal_outcome)
    .execute(pool).await.expect("seed");
    (exec_uuid, eid)
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn eligible_when_standalone_runnable_unowned() {
    let backend = fresh_backend().await;
    let (_u, eid) = seed_exec(&backend, "runnable", "unowned", None, "none").await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await.expect("ok");
    assert_eq!(status, "eligible");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn not_found_without_row() {
    let backend = fresh_backend().await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let bogus = ExecutionId::parse("{fp:0}:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: bogus })
        .await.expect("ok");
    assert_eq!(status, "not_found");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn not_runnable_when_active() {
    let backend = fresh_backend().await;
    let (_u, eid) = seed_exec(&backend, "active", "leased", None, "none").await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await.expect("ok");
    assert_eq!(status, "not_runnable");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn owned_when_ownership_not_unowned() {
    let backend = fresh_backend().await;
    let (_u, eid) = seed_exec(&backend, "runnable", "leased", None, "none").await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: eid })
        .await.expect("ok");
    assert_eq!(status, "owned");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn blocked_by_required_edge_with_non_terminal_upstream() {
    let backend = fresh_backend().await;
    let flow_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, public_flow_state, created_at_ms) \
         VALUES (0, ?1, 'running', ?2)",
    )
    .bind(flow_id).bind(now_ms()).execute(backend.pool_for_test()).await.expect("seed flow");
    let (up_u, _) = seed_exec(&backend, "runnable", "unowned", Some(flow_id), "none").await;
    let (down_u, down_eid) = seed_exec(&backend, "runnable", "unowned", Some(flow_id), "none").await;
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES (0, ?1, ?2, ?3, ?4, '{\"kind\":\"required\"}')",
    )
    .bind(flow_id).bind(Uuid::new_v4()).bind(up_u).bind(down_u)
    .execute(backend.pool_for_test()).await.expect("seed edge");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: down_eid })
        .await.expect("ok");
    assert_eq!(status, "blocked_by_dependencies");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn impossible_when_required_upstream_failed() {
    let backend = fresh_backend().await;
    let flow_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, public_flow_state, created_at_ms) \
         VALUES (0, ?1, 'running', ?2)",
    )
    .bind(flow_id).bind(now_ms()).execute(backend.pool_for_test()).await.expect("seed flow");
    let (up_u, _) = seed_exec(&backend, "terminal", "unowned", Some(flow_id), "failed").await;
    let (down_u, down_eid) = seed_exec(&backend, "runnable", "unowned", Some(flow_id), "none").await;
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES (0, ?1, ?2, ?3, ?4, '{\"kind\":\"required\"}')",
    )
    .bind(flow_id).bind(Uuid::new_v4()).bind(up_u).bind(down_u)
    .execute(backend.pool_for_test()).await.expect("seed edge");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let EvaluateFlowEligibilityResult::Status { status } = be
        .evaluate_flow_eligibility(EvaluateFlowEligibilityArgs { execution_id: down_eid })
        .await.expect("ok");
    assert_eq!(status, "impossible");
}
