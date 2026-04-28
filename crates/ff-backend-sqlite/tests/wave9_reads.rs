//! RFC-023 Phase 3.3 — Wave 9 read-model integration tests.
//!
//! Covers `read_execution_state`, `read_execution_info`, and
//! `get_execution_result` on the SQLite backend: happy path, missing
//! row → `Ok(None)`, lifecycle_phase normalisation, terminal-outcome
//! derivation, current-attempt semantics for `get_execution_result`.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy};
use ff_core::contracts::{CreateExecutionArgs, CreateExecutionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::state::{LifecyclePhase, PublicState, TerminalOutcome};
use ff_core::types::{
    ExecutionId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use serial_test::serial;
use uuid::Uuid;

// ── Shared setup ───────────────────────────────────────────────────

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
    let uri = format!("file:rfc-023-wave9-reads-{}?mode=memory&cache=shared", uuid_like());
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
    let r = b.create_execution(args).await.expect("create");
    assert!(matches!(r, CreateExecutionResult::Created { .. }));
    // Promote to runnable so claim() picks it up.
    let exec_uuid = uuid_of(&exec_id);
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    exec_id
}

async fn create_and_claim(b: &Arc<SqliteBackend>) -> ExecutionId {
    let lane = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = create_runnable(b, &lane).await;
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let _ = b
        .claim(&lane, &CapabilitySet::default(), policy)
        .await
        .expect("claim")
        .expect("handle");
    exec_id
}

// ── read_execution_state ───────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_state_missing_returns_none() {
    let b = fresh_backend().await;
    let missing = new_exec_id();
    let got = b.read_execution_state(&missing).await.expect("ok");
    assert!(got.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_state_happy_path_submitted() {
    // Fresh create → public_state='waiting' (per INSERT_EXEC_CORE_SQL).
    let b = fresh_backend().await;
    let lane = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = {
        let id = new_exec_id();
        let args = CreateExecutionArgs {
            execution_id: id.clone(),
            namespace: Namespace::new("default"),
            lane_id: lane,
            execution_kind: "op".into(),
            input_payload: vec![],
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
        let _ = b.create_execution(args).await.expect("create");
        id
    };
    let got = b.read_execution_state(&exec_id).await.expect("ok").expect("some");
    assert_eq!(got, PublicState::Waiting);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_state_normalizes_running_to_active() {
    // Verify the normaliser collapses the storage-tier `running`
    // literal to `PublicState::Active` (matches PG exec_core
    // normalisation). SQLite's claim path does not itself write
    // public_state='running' today; seed the literal directly so the
    // test exercises the normaliser regardless of the write-site gap.
    let b = fresh_backend().await;
    let exec_id = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    sqlx::query(
        "UPDATE ff_exec_core SET public_state='running' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    let got = b.read_execution_state(&exec_id).await.expect("ok").expect("some");
    assert_eq!(got, PublicState::Active);
}

// ── read_execution_info ────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_info_missing_returns_none() {
    let b = fresh_backend().await;
    let missing = new_exec_id();
    assert!(b.read_execution_info(&missing).await.expect("ok").is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_info_happy_path_post_claim() {
    let b = fresh_backend().await;
    let exec_id = create_and_claim(&b).await;
    let info = b
        .read_execution_info(&exec_id)
        .await
        .expect("ok")
        .expect("some");
    assert_eq!(info.execution_id, exec_id);
    assert_eq!(info.namespace, "default");
    assert_eq!(info.execution_kind, "op");
    // SQLite's claim path writes `public_state = 'running'` (the
    // Postgres-parity literal from `suspend_ops.rs:958-960`), which
    // the Spine-B normaliser maps to `PublicState::Active`.
    assert_eq!(info.public_state, PublicState::Active);
    assert_eq!(info.state_vector.lifecycle_phase, LifecyclePhase::Active);
    assert_eq!(
        info.state_vector.terminal_outcome,
        TerminalOutcome::None,
        "active row is not terminal"
    );
    assert!(info.started_at.is_some(), "claim set started_at on attempt 0");
    assert!(info.completed_at.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_execution_info_terminal_cancelled_derives_outcome() {
    use ff_core::contracts::{CancelExecutionArgs, CancelExecutionResult};
    use ff_core::types::CancelSource;

    let b = fresh_backend().await;
    let exec_id = create_and_claim(&b).await;
    let _ = b
        .cancel_execution(CancelExecutionArgs {
            execution_id: exec_id.clone(),
            reason: "test".into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::from_millis(now_ms()),
        })
        .await
        .expect("cancel");

    let info = b
        .read_execution_info(&exec_id)
        .await
        .expect("ok")
        .expect("some");
    assert_eq!(info.state_vector.lifecycle_phase, LifecyclePhase::Terminal);
    assert_eq!(info.public_state, PublicState::Cancelled);
    assert_eq!(
        info.state_vector.terminal_outcome,
        TerminalOutcome::Cancelled,
        "lifecycle_phase='cancelled' → TerminalOutcome::Cancelled",
    );
    assert!(info.completed_at.is_some(), "terminal_at_ms populated by cancel");
    let _ = matches!(info.state_vector.lifecycle_phase, LifecyclePhase::Terminal);
    let _ = CancelExecutionResult::Cancelled {
        execution_id: exec_id.clone(),
        public_state: PublicState::Cancelled,
    };
}

// ── get_execution_result ───────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn get_execution_result_missing_returns_none() {
    let b = fresh_backend().await;
    let missing = new_exec_id();
    let got = b.get_execution_result(&missing).await.expect("ok");
    assert!(got.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn get_execution_result_active_returns_none() {
    // An active execution has no stored result yet.
    let b = fresh_backend().await;
    let exec_id = create_and_claim(&b).await;
    let got = b.get_execution_result(&exec_id).await.expect("ok");
    assert!(got.is_none(), "active exec has no result payload");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn get_execution_result_terminal_returns_payload() {
    // Seed the `result` column directly (matches the PG reference
    // test's style — the real write path is `complete()` which Phase
    // 3.3 is not scoped to exercise).
    let b = fresh_backend().await;
    let exec_id = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    let payload = b"the-result-bytes".to_vec();
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal', public_state='completed', \
         result=?1 WHERE partition_key=0 AND execution_id=?2",
    )
    .bind(&payload)
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let got = b.get_execution_result(&exec_id).await.expect("ok").expect("some");
    assert_eq!(got, payload);
}
