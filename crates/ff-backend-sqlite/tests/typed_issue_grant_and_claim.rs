//! cairn #454 Phase 5 — SQLite coverage of
//! `EngineBackend::issue_grant_and_claim`. Parity with the PG tests at
//! `ff-backend-postgres/tests/typed_issue_grant_and_claim.rs`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::IssueGrantAndClaimArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::types::{ExecutionId, LaneId};
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
    let uri = format!(
        "file:typed-issue-grant-{}?mode=memory&cache=shared",
        uuid_like()
    );
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
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec");
    (exec_uuid, eid)
}

fn args_for(eid: ExecutionId) -> IssueGrantAndClaimArgs {
    IssueGrantAndClaimArgs::new(eid, LaneId::new("default"), 60_000)
}

async fn count_grants(backend: &SqliteBackend, exec_uuid: Uuid) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(backend.pool_for_test())
    .await
    .unwrap();
    n
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_grant_and_claim_happy_path() {
    let be = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&be).await;
    let eb: Arc<dyn EngineBackend> = be.clone();
    let outcome = eb
        .issue_grant_and_claim(args_for(eid))
        .await
        .expect("issue_grant_and_claim ok");
    assert_eq!(outcome.lease_epoch.0, 1, "fresh claim starts at epoch 1");
    assert_eq!(outcome.attempt_index.0, 0, "fresh claim at attempt 0");
    assert!(!outcome.lease_id.to_string().is_empty());

    let row = sqlx::query(
        "SELECT lifecycle_phase, ownership_state, public_state, attempt_state, attempt_index \
         FROM ff_exec_core WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(be.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "active");
    assert_eq!(row.get::<String, _>("ownership_state"), "leased");
    assert_eq!(row.get::<String, _>("public_state"), "running");
    assert_eq!(row.get::<String, _>("attempt_state"), "running_attempt");
    assert_eq!(row.get::<i64, _>("attempt_index"), 1);

    assert_eq!(count_grants(&be, exec_uuid).await, 0, "grant cleaned up");

    let (worker_id, lease_epoch_i): (String, i64) = sqlx::query_as(
        "SELECT worker_id, lease_epoch FROM ff_attempt \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = 0",
    )
    .bind(exec_uuid)
    .fetch_one(be.pool_for_test())
    .await
    .unwrap();
    assert_eq!(worker_id, "operator");
    assert_eq!(lease_epoch_i, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_grant_and_claim_rejects_already_leased() {
    let be = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&be).await;
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state = 'leased' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(be.pool_for_test())
    .await
    .unwrap();
    let eb: Arc<dyn EngineBackend> = be.clone();
    match eb.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::Contention(ContentionKind::LeaseConflict)) => {}
        other => panic!("expected LeaseConflict, got {other:?}"),
    }
    assert_eq!(
        count_grants(&be, exec_uuid).await,
        0,
        "rejected composition must not leak a grant row"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_grant_and_claim_rejects_missing_execution() {
    let be = fresh_backend().await;
    let fake_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:0}}:{fake_uuid}")).unwrap();
    let eb: Arc<dyn EngineBackend> = be.clone();
    match eb.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::NotFound { entity: "execution" }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert_eq!(count_grants(&be, fake_uuid).await, 0);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_grant_and_claim_no_dangling_grant_on_reject() {
    let be = fresh_backend().await;
    let (exec_uuid, eid) = seed_runnable(&be).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(be.pool_for_test())
    .await
    .unwrap();
    let eb: Arc<dyn EngineBackend> = be.clone();
    match eb.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
    assert_eq!(count_grants(&be, exec_uuid).await, 0);
}
