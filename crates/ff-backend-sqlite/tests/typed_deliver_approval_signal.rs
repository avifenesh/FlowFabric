//! cairn #454 Phase 5 — SQLite coverage of
//! `EngineBackend::deliver_approval_signal`. Parity with the PG tests
//! at `ff-backend-postgres/tests/typed_deliver_approval_signal.rs`.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy, Handle};
use ff_core::contracts::{
    CreateExecutionArgs, CreateExecutionResult, DeliverApprovalSignalArgs, DeliverSignalResult,
    ResumeCondition, ResumePolicy, SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::{
    ExecutionId, LaneId, Namespace, SuspensionId, TimestampMs, WaitpointId, WorkerId,
    WorkerInstanceId,
};
use serial_test::serial;
use uuid::Uuid;

const KID: &str = "kid-approval";
fn secret_hex() -> String {
    "cafebabe".repeat(8)
}

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
        "file:typed-deliver-approval-{}?mode=memory&cache=shared",
        uuid_like()
    );
    let b = SqliteBackend::new(&uri).await.expect("backend");
    b.seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed kid");
    b
}

async fn create_and_claim(b: &Arc<SqliteBackend>) -> (ExecutionId, Handle) {
    let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id");
    let args = CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: LaneId::new("default"),
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
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', public_state='pending', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let handle = b
        .claim(&LaneId::new("default"), &CapabilitySet::default(), policy)
        .await
        .expect("claim")
        .expect("handle");
    (exec_id, handle)
}

async fn suspend_on(
    b: &Arc<SqliteBackend>,
    handle: &Handle,
    wp_key: &str,
    signal_name: &str,
) -> WaitpointId {
    let wp_id = WaitpointId::new();
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: wp_key.to_owned(),
    };
    let cond = ResumeCondition::Single {
        waitpoint_key: wp_key.to_owned(),
        matcher: SignalMatcher::ByName(signal_name.to_owned()),
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::from_millis(now_ms()),
    );
    let _ = b.suspend(handle, args).await.expect("suspend");
    wp_id
}

fn approval_args(
    exec_id: &ExecutionId,
    lane: &LaneId,
    wp_id: &WaitpointId,
    signal_name: &str,
    suffix: &str,
) -> DeliverApprovalSignalArgs {
    DeliverApprovalSignalArgs::new(
        exec_id.clone(),
        lane.clone(),
        wp_id.clone(),
        signal_name,
        suffix,
        60_000,
        None,
        None,
    )
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn approved_path_resumes_execution() {
    let b = fresh_backend().await;
    let lane = LaneId::new("default");
    let (exec_id, handle) = create_and_claim(&b).await;
    let wp_id = suspend_on(&b, &handle, "wpk:approval:ok", "approved").await;
    let args = approval_args(&exec_id, &lane, &wp_id, "approved", "dec-1");
    let eb: Arc<dyn EngineBackend> = b.clone();
    let out = eb
        .deliver_approval_signal(args)
        .await
        .expect("deliver_approval_signal ok");
    match out {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn rejected_path_appends_without_resume() {
    let b = fresh_backend().await;
    let lane = LaneId::new("default");
    let (exec_id, handle) = create_and_claim(&b).await;
    // Condition requires "approved"; delivering "rejected" should be
    // appended but does not satisfy the resume condition.
    let wp_id = suspend_on(&b, &handle, "wpk:approval:reject", "approved").await;
    let args = approval_args(&exec_id, &lane, &wp_id, "rejected", "dec-1");
    let eb: Arc<dyn EngineBackend> = b.clone();
    let out = eb
        .deliver_approval_signal(args)
        .await
        .expect("deliver_approval_signal ok");
    match out {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn missing_waitpoint_returns_not_found() {
    let b = fresh_backend().await;
    let lane = LaneId::new("default");
    let (exec_id, _handle) = create_and_claim(&b).await;
    let bogus = WaitpointId::new();
    let args = approval_args(&exec_id, &lane, &bogus, "approved", "dec-1");
    let eb: Arc<dyn EngineBackend> = b.clone();
    match eb.deliver_approval_signal(args).await.unwrap_err() {
        EngineError::NotFound { entity } => assert_eq!(entity, "waitpoint"),
        other => panic!("expected NotFound(waitpoint), got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn lane_mismatch_is_validation_error() {
    let b = fresh_backend().await;
    let lane = LaneId::new("default");
    let (exec_id, handle) = create_and_claim(&b).await;
    let wp_id = suspend_on(&b, &handle, "wpk:approval:lane", "approved").await;
    let wrong_lane = LaneId::new("not-default");
    let args = approval_args(&exec_id, &wrong_lane, &wp_id, "approved", "dec-1");
    let _ = lane;
    let eb: Arc<dyn EngineBackend> = b.clone();
    match eb.deliver_approval_signal(args).await.unwrap_err() {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert!(
                detail.starts_with("lane_mismatch:"),
                "unexpected detail: {detail}"
            );
        }
        other => panic!("expected Validation(InvalidInput), got {other:?}"),
    }
}
