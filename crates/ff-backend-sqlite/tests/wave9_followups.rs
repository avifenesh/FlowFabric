//! RFC-020 Wave 9 follow-ups (#355, #356) — SQLite regression coverage.
//!
//! PG parallels live in
//! `crates/ff-backend-postgres/tests/wave9_followups.rs`. #354 is
//! doc-only.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy};
use ff_core::contracts::{CancelFlowResult, CreateExecutionArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{
    ExecutionId, FlowId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use serial_test::serial;
use sqlx::Row;
use uuid::Uuid;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
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
        "file:rfc-023-wave9-followups-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("backend")
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

fn uuid_of(eid: &ExecutionId) -> Uuid {
    Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
}

async fn create_runnable(b: &Arc<SqliteBackend>, lane: &LaneId) -> ExecutionId {
    let exec_id = new_exec_id();
    let args = CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: lane.clone(),
        execution_kind: "op".into(),
        input_payload: b"hello".to_vec(),
        payload_encoding: None,
        priority: 0,
        creator_identity: "test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(now_ms()),
    };
    b.create_execution(args).await.expect("create");

    // Force the row into claimable state (mirrors wave9_operator's helper).
    let exec_uuid = uuid_of(&exec_id);
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', eligibility_state='eligible_now', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    exec_id
}

async fn read_started_at_ms(b: &Arc<SqliteBackend>, exec_uuid: Uuid) -> Option<i64> {
    let row = sqlx::query(
        "SELECT started_at_ms FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    row.try_get::<Option<i64>, _>("started_at_ms").unwrap()
}

// ── #356 — started_at_ms set-once ────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn sqlite_first_claim_populates_started_at_ms_set_once() {
    let b = fresh_backend().await;
    let lane_id = LaneId::new(format!("wave9-356-{}", uuid_like()).as_str());
    let exec_id = create_runnable(&b, &lane_id).await;
    let exec_uuid = uuid_of(&exec_id);

    // Pre: column is NULL on freshly created rows.
    assert_eq!(read_started_at_ms(&b, exec_uuid).await, None);

    let policy = ClaimPolicy::new(
        WorkerId::new("w356"),
        WorkerInstanceId::new("w356-i1"),
        30_000,
        None,
    );

    let _h1 = b
        .claim(&lane_id, &CapabilitySet::default(), policy.clone())
        .await
        .expect("claim 1")
        .expect("handle 1");
    let first = read_started_at_ms(&b, exec_uuid).await.expect("populated");

    // Force a reclaim-like cycle: put the row back to runnable + bump
    // attempt_index, reclaim, and verify started_at_ms is unchanged.
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', ownership_state='unowned', \
         eligibility_state='eligible_now', attempt_state='initial', \
         attempt_index=attempt_index+1 \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let _h2 = b
        .claim(&lane_id, &CapabilitySet::default(), policy)
        .await
        .expect("claim 2")
        .expect("handle 2");
    let second = read_started_at_ms(&b, exec_uuid).await.expect("populated");

    assert_eq!(first, second, "set-once: reclaim must not overwrite started_at_ms (#356)");
}

// ── #355 — cancel_flow clears member attempt.outcome ─────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn sqlite_cancel_flow_clears_member_attempt_outcome() {
    let b = fresh_backend().await;
    let lane_id = LaneId::new("wave9-355-cancel-flow");
    let exec_id = create_runnable(&b, &lane_id).await;
    let exec_uuid = uuid_of(&exec_id);

    // Seed a flow header + bind the exec to it.
    let flow_id = FlowId::new();
    let flow_uuid = flow_id.0;
    sqlx::query(
        "INSERT INTO ff_flow_core \
           (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES (0, ?1, 0, 'open', ?2)",
    )
    .bind(flow_uuid)
    .bind(now_ms())
    .execute(b.pool_for_test())
    .await
    .unwrap();

    sqlx::query("UPDATE ff_exec_core SET flow_id=?1 WHERE partition_key=0 AND execution_id=?2")
        .bind(flow_uuid)
        .bind(exec_uuid)
        .execute(b.pool_for_test())
        .await
        .unwrap();

    // Seed a stale `retry` outcome on the current attempt.
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, lease_epoch, outcome) \
         VALUES (0, ?1, 0, 1, 'retry')",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    // Pre: stale outcome is observable.
    let pre: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_attempt WHERE partition_key=0 AND execution_id=?1 AND attempt_index=0",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(pre.as_deref(), Some("retry"));

    let r = b
        .cancel_flow(&flow_id, CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait)
        .await
        .expect("cancel_flow ok");
    assert!(
        matches!(r, CancelFlowResult::Cancelled { .. }),
        "got {r:?}"
    );

    // Post: outcome nulled (#355).
    let post: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_attempt WHERE partition_key=0 AND execution_id=?1 AND attempt_index=0",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(post, None, "post-cancel: attempt.outcome must be NULL (#355)");
}
