//! RFC-023 Phase 3.2 — Wave 9 operator-control integration tests.
//!
//! Covers `cancel_execution`, `revoke_lease`, `change_priority`, and
//! `replay_execution` on the SQLite backend: happy path, fence /
//! gate failures, outbox emission, and the replay branch selector
//! (normal vs skipped_flow_member).

#![cfg(feature = "core")]

use std::sync::Arc;
use std::time::Duration;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy};
use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, ChangePriorityArgs, ChangePriorityResult,
    CreateExecutionArgs, CreateExecutionResult, ReplayExecutionArgs, ReplayExecutionResult,
    RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind};
use ff_core::state::PublicState;
use ff_core::types::{
    CancelSource, ExecutionId, LaneId, LeaseEpoch, Namespace, TimestampMs, WorkerId,
    WorkerInstanceId,
};
use serial_test::serial;
use uuid::Uuid;

// ── Setup helpers ──────────────────────────────────────────────────

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
    // SAFETY: test-only env; all callers are serial-gated.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:rfc-023-wave9-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct backend")
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

fn create_args(exec_id: &ExecutionId, lane_id: &LaneId) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: lane_id.clone(),
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
    }
}

/// Create a runnable exec. Mirrors the claim-path pre-setup used by
/// `tests/subscribe.rs`: after create_execution the row is in a
/// `pending`/`initial` shape that `claim()` does not yet route; force
/// it to `runnable` + `pending` so a claim call finds it.
async fn create_runnable(b: &Arc<SqliteBackend>) -> (ExecutionId, LaneId) {
    let lane_id = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = new_exec_id();
    let args = create_args(&exec_id, &lane_id);
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
    (exec_id, lane_id)
}

async fn create_and_claim(
    b: &Arc<SqliteBackend>,
) -> (ExecutionId, ff_core::backend::Handle) {
    let (exec_id, lane_id) = create_runnable(b).await;
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let handle = b
        .claim(&lane_id, &CapabilitySet::default(), policy)
        .await
        .expect("claim")
        .expect("handle");
    (exec_id, handle)
}

fn uuid_of(eid: &ExecutionId) -> Uuid {
    Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
}

async fn count_outbox(
    b: &Arc<SqliteBackend>,
    table: &str,
    exec_uuid: Uuid,
) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {table} WHERE execution_id = ?1 AND partition_key = 0"
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(exec_uuid.to_string())
        .fetch_one(b.pool_for_test())
        .await
        .unwrap()
}

async fn read_lifecycle_phase(b: &Arc<SqliteBackend>, exec_uuid: Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap()
}

// ── cancel_execution ───────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn cancel_execution_happy_path() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);

    let args = CancelExecutionArgs {
        execution_id: exec_id.clone(),
        reason: "operator test".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.cancel_execution(args).await.expect("cancel");
    assert!(matches!(
        r,
        CancelExecutionResult::Cancelled {
            public_state: PublicState::Cancelled,
            ..
        }
    ));

    // exec_core flipped to cancelled.
    assert_eq!(read_lifecycle_phase(&b, exec_uuid).await, "cancelled");

    // Per RFC-020 §4.2.7 matrix, cancel_execution emits ONLY the
    // lease_event (`revoked`) when a lease was active — no
    // ff_operator_event row. acquired (from claim) + revoked (from
    // cancel) = 2 lease_event rows.
    assert_eq!(count_outbox(&b, "ff_operator_event", exec_uuid).await, 0);
    assert_eq!(count_outbox(&b, "ff_lease_event", exec_uuid).await, 2,
        "acquired + revoked lease events expected");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn cancel_execution_idempotent_when_already_cancelled() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;

    let args = CancelExecutionArgs {
        execution_id: exec_id.clone(),
        reason: "first".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    let _ = b.cancel_execution(args.clone()).await.expect("cancel 1");
    // Second call — idempotent replay.
    let r = b.cancel_execution(args).await.expect("cancel 2");
    assert!(matches!(r, CancelExecutionResult::Cancelled { .. }));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn cancel_execution_lease_fence_required_when_not_operator_override() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;

    // source != OperatorOverride + lease active + missing lease_epoch ⇒
    // Validation error per RFC-020 §4.2.1.
    let args = CancelExecutionArgs {
        execution_id: exec_id.clone(),
        reason: "no fence".into(),
        source: CancelSource::LeaseHolder,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    let err = b.cancel_execution(args).await.unwrap_err();
    assert!(matches!(err, EngineError::Validation { .. }), "got {err:?}");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn cancel_execution_stale_lease_fence_rejects() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;

    // Wrong lease_epoch — triggers StaleLease.
    let args = CancelExecutionArgs {
        execution_id: exec_id.clone(),
        reason: "bad fence".into(),
        source: CancelSource::LeaseHolder,
        lease_id: None,
        lease_epoch: Some(LeaseEpoch(999_999)),
        attempt_id: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    let err = b.cancel_execution(args).await.unwrap_err();
    assert!(
        matches!(err, EngineError::State(StateKind::StaleLease)),
        "got {err:?}"
    );
}

// ── revoke_lease ───────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn revoke_lease_happy_path() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);

    let args = RevokeLeaseArgs {
        execution_id: exec_id.clone(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("w1-i1"),
        reason: "operator revoke".into(),
    };
    let r = b.revoke_lease(args).await.expect("revoke");
    assert!(matches!(r, RevokeLeaseResult::Revoked { .. }), "got {r:?}");

    // Lease cleared — another acquired + revoked pair in the outbox.
    assert_eq!(
        count_outbox(&b, "ff_lease_event", exec_uuid).await,
        2,
        "expected acquired (from claim) + revoked (from revoke_lease)"
    );

    // exec_core back to runnable (reclaimable).
    assert_eq!(read_lifecycle_phase(&b, exec_uuid).await, "runnable");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn revoke_lease_no_active_lease_returns_already_satisfied() {
    let b = fresh_backend().await;
    let (exec_id, _lane) = create_runnable(&b).await;

    let args = RevokeLeaseArgs {
        execution_id: exec_id.clone(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("w1-i1"),
        reason: "no lease".into(),
    };
    let r = b.revoke_lease(args).await.expect("revoke");
    match r {
        RevokeLeaseResult::AlreadySatisfied { reason } => {
            assert_eq!(reason, "no_active_lease");
        }
        other => panic!("expected AlreadySatisfied, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn revoke_lease_different_worker_returns_already_satisfied() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;

    // Caller targets a different wiid — idempotent skip.
    let args = RevokeLeaseArgs {
        execution_id: exec_id.clone(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("w2-i1"),
        reason: "wrong worker".into(),
    };
    let r = b.revoke_lease(args).await.expect("revoke");
    match r {
        RevokeLeaseResult::AlreadySatisfied { reason } => {
            assert_eq!(reason, "different_worker_instance_id");
        }
        other => panic!("expected AlreadySatisfied, got {other:?}"),
    }
}

// ── change_priority ────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn change_priority_happy_path() {
    let b = fresh_backend().await;
    let (exec_id, _lane) = create_runnable(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    // eligible_now requires a secondary flip — mimic the Valkey-canonical
    // state the RFC gates on. create_runnable flips to runnable + pending;
    // set eligibility_state = 'eligible_now' directly.
    sqlx::query(
        "UPDATE ff_exec_core SET eligibility_state='eligible_now' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let args = ChangePriorityArgs {
        execution_id: exec_id.clone(),
        new_priority: 42,
        lane_id: LaneId::new("lane-ignored-on-sqlite"),
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.change_priority(args).await.expect("change_priority");
    assert!(matches!(r, ChangePriorityResult::Changed { .. }));

    let p: i64 = sqlx::query_scalar(
        "SELECT priority FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(p, 42);

    assert_eq!(count_outbox(&b, "ff_operator_event", exec_uuid).await, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn change_priority_ineligible_execution_returns_not_eligible() {
    let b = fresh_backend().await;
    // Create exec but leave it in the default state (pending, not
    // eligible_now). Expect ExecutionNotEligible.
    let lane = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = new_exec_id();
    b.create_execution(create_args(&exec_id, &lane)).await.expect("create");

    let args = ChangePriorityArgs {
        execution_id: exec_id.clone(),
        new_priority: 10,
        lane_id: lane,
        now: TimestampMs::from_millis(now_ms()),
    };
    let err = b.change_priority(args).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::ExecutionNotEligible)),
        "got {err:?}"
    );
}

// ── replay_execution ───────────────────────────────────────────────

/// Helper: drive an exec all the way to terminal so replay has
/// something to lift back to runnable.
async fn create_complete_terminal(
    b: &Arc<SqliteBackend>,
) -> (ExecutionId, Uuid) {
    let (exec_id, handle) = create_and_claim(b).await;
    b.complete(&handle, None).await.expect("complete");
    // complete sets lifecycle_phase='terminal'; confirm.
    let exec_uuid = uuid_of(&exec_id);
    assert_eq!(read_lifecycle_phase(b, exec_uuid).await, "terminal");
    (exec_id, exec_uuid)
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn replay_execution_normal_branch() {
    let b = fresh_backend().await;
    let (exec_id, exec_uuid) = create_complete_terminal(&b).await;

    let args = ReplayExecutionArgs {
        execution_id: exec_id.clone(),
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.replay_execution(args).await.expect("replay");
    assert!(
        matches!(
            r,
            ReplayExecutionResult::Replayed {
                public_state: PublicState::Waiting,
            }
        ),
        "normal branch must resume to Waiting, got {r:?}"
    );

    // lifecycle_phase flipped back to runnable.
    assert_eq!(read_lifecycle_phase(&b, exec_uuid).await, "runnable");
    // raw_fields.replay_count bumped to 1 (stored as JSON number).
    let replay_count: i64 = sqlx::query_scalar(
        "SELECT json_extract(raw_fields, '$.replay_count') \
         FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(replay_count, 1);
    // operator_event 'replayed' emitted.
    assert_eq!(count_outbox(&b, "ff_operator_event", exec_uuid).await, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn replay_execution_skipped_flow_member_resets_edge_group_counters() {
    let b = fresh_backend().await;
    let (exec_id, exec_uuid) = create_complete_terminal(&b).await;

    // Force the skipped-flow-member branch: mark the attempt outcome
    // as 'skipped' and attach a flow_id. Pre-populate ff_edge +
    // ff_edge_group with non-zero skip/fail/running counts so the
    // reset is observable.
    let fake_flow = Uuid::new_v4();
    sqlx::query(
        "UPDATE ff_exec_core SET flow_id=?1 \
         WHERE partition_key=0 AND execution_id=?2",
    )
    .bind(fake_flow)
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE ff_attempt SET outcome='skipped' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    let edge_id = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, \
         downstream_eid, policy, policy_version) \
         VALUES (0, ?1, ?2, ?3, ?4, '{}', 0)",
    )
    .bind(fake_flow)
    .bind(edge_id)
    .bind(upstream)
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, \
         policy, k_target, success_count, fail_count, skip_count, running_count) \
         VALUES (0, ?1, ?2, '{}', 1, 3, 5, 7, 2)",
    )
    .bind(fake_flow)
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let args = ReplayExecutionArgs {
        execution_id: exec_id.clone(),
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.replay_execution(args).await.expect("replay");
    assert!(
        matches!(
            r,
            ReplayExecutionResult::Replayed {
                public_state: PublicState::WaitingChildren,
            }
        ),
        "skipped-flow-member branch resumes to WaitingChildren, got {r:?}"
    );

    // Counters reset — success preserved (Option A: satisfied edges
    // remain satisfied).
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT success_count, fail_count, skip_count, running_count \
         FROM ff_edge_group WHERE partition_key=0 AND flow_id=?1 AND downstream_eid=?2",
    )
    .bind(fake_flow)
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(row.0, 3, "success_count preserved");
    assert_eq!(row.1, 0, "fail_count reset");
    assert_eq!(row.2, 0, "skip_count reset");
    assert_eq!(row.3, 0, "running_count reset");

    // operator_event `replayed` emitted with branch=skipped_flow_member.
    let details: String = sqlx::query_scalar(
        "SELECT details FROM ff_operator_event \
         WHERE execution_id=?1 AND event_type='replayed'",
    )
    .bind(exec_uuid.to_string())
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert!(
        details.contains("skipped_flow_member"),
        "details should mark skipped_flow_member branch: {details}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn replay_execution_non_terminal_returns_not_terminal() {
    let b = fresh_backend().await;
    let (exec_id, _lane) = create_runnable(&b).await;

    let args = ReplayExecutionArgs {
        execution_id: exec_id.clone(),
        now: TimestampMs::from_millis(now_ms()),
    };
    let err = b.replay_execution(args).await.unwrap_err();
    assert!(
        matches!(err, EngineError::State(StateKind::ExecutionNotTerminal)),
        "got {err:?}"
    );
}

// ── concurrent cancel retry ────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn concurrent_cancel_serialization() {
    let b = fresh_backend().await;
    let (exec_id, _handle) = create_and_claim(&b).await;

    let args = CancelExecutionArgs {
        execution_id: exec_id.clone(),
        reason: "concurrent".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    // Two concurrent cancels — both must resolve OK (one wins the
    // first-cancel path, the second observes the terminal state and
    // returns Cancelled idempotently). SQLite's BEGIN IMMEDIATE
    // serializes; the retry wrapper absorbs any SQLITE_BUSY under load.
    let b2 = b.clone();
    let args2 = args.clone();
    let (r1, r2) = tokio::join!(
        b.cancel_execution(args),
        b2.cancel_execution(args2),
    );
    assert!(
        matches!(r1, Ok(CancelExecutionResult::Cancelled { .. })),
        "first cancel must succeed: {r1:?}"
    );
    assert!(
        matches!(r2, Ok(CancelExecutionResult::Cancelled { .. })),
        "second cancel must idempotently succeed: {r2:?}"
    );
    // Small delay to let any broadcast emits settle before the backend
    // drops (avoids a noisy test-teardown race on the wakeup channel).
    tokio::time::sleep(Duration::from_millis(20)).await;
}
