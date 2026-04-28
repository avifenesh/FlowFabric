//! RFC-024 PR-E — SQLite `issue_reclaim_grant` + `reclaim_execution`
//! integration tests.
//!
//! Mirrors the shape of `tests/wave9_operator.rs` (FF_DEV_MODE=1
//! shared-cache in-memory backend, `#[serial(ff_dev_mode)]` gating).
//! Parallel to `ff-backend-postgres/tests/rfc024_reclaim.rs` (PR-D).

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy, HandleKind};
use ff_core::contracts::{
    CreateExecutionArgs, CreateExecutionResult, IssueReclaimGrantArgs, IssueReclaimGrantOutcome,
    ReclaimExecutionArgs, ReclaimExecutionOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseId, Namespace, TimestampMs, WorkerId,
    WorkerInstanceId,
};
use serial_test::serial;
use uuid::Uuid;

// -- Setup helpers (mirrors wave9_operator.rs) --------------------

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
    // SAFETY: test-only env mutation; `#[serial(ff_dev_mode)]` gates
    // every reader + writer across this crate's test binaries.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:rfc-024-reclaim-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct backend")
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

fn uuid_of(eid: &ExecutionId) -> Uuid {
    Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
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

async fn create_runnable(b: &Arc<SqliteBackend>) -> (ExecutionId, LaneId) {
    let lane_id = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = new_exec_id();
    let args = create_args(&exec_id, &lane_id);
    let r = b.create_execution(args).await.expect("create");
    assert!(matches!(r, CreateExecutionResult::Created { .. }));
    let exec_uuid = uuid_of(&exec_id);
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
) -> (ExecutionId, LaneId, ff_core::backend::Handle) {
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
    (exec_id, lane_id, handle)
}

/// Force `ownership_state = 'lease_expired_reclaimable'` on an
/// already-claimed execution — mirrors the `mark_expired` scanner
/// arriving after TTL without actually waiting the TTL out.
async fn force_lease_expired(b: &Arc<SqliteBackend>, exec_uuid: Uuid) {
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='lease_expired_reclaimable' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
}

fn issue_args(exec_id: &ExecutionId, lane_id: &LaneId) -> IssueReclaimGrantArgs {
    IssueReclaimGrantArgs::new(
        exec_id.clone(),
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i2"),
        lane_id.clone(),
        None,
        60_000,
        None,
        None,
        Default::default(),
        TimestampMs::from_millis(now_ms()),
    )
}

#[allow(clippy::too_many_arguments)]
fn reclaim_args(
    exec_id: &ExecutionId,
    lane_id: &LaneId,
    worker_id: &str,
    worker_instance_id: &str,
    current_attempt_index: u32,
    max_reclaim_count: Option<u32>,
) -> ReclaimExecutionArgs {
    ReclaimExecutionArgs::new(
        exec_id.clone(),
        WorkerId::new(worker_id),
        WorkerInstanceId::new(worker_instance_id),
        lane_id.clone(),
        None,
        LeaseId::new(),
        30_000,
        AttemptId::new(),
        String::new(),
        max_reclaim_count,
        WorkerInstanceId::new("w1-i1"),
        AttemptIndex::new(current_attempt_index),
    )
}

// -- issue_reclaim_grant ----------------------------------------------

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_reclaim_grant_happy_path() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let r = b.issue_reclaim_grant(issue_args(&exec_id, &lane_id)).await.expect("issue");
    let grant = match r {
        IssueReclaimGrantOutcome::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    assert_eq!(grant.execution_id, exec_id);
    assert!(!grant.grant_key.is_empty(), "grant_key must be populated");
    assert!(grant.expires_at_ms > 0);

    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant WHERE partition_key=0 \
         AND execution_id=?1 AND kind='reclaim'",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_reclaim_grant_wrong_phase() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    // Execution is `active` + `leased` (not lease_expired_reclaimable).

    let r = b.issue_reclaim_grant(issue_args(&exec_id, &lane_id)).await.expect("issue");
    assert!(
        matches!(r, IssueReclaimGrantOutcome::NotReclaimable { .. }),
        "got {r:?}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn issue_reclaim_grant_lease_revoked_admits() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='lease_revoked' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let r = b.issue_reclaim_grant(issue_args(&exec_id, &lane_id)).await.expect("issue");
    assert!(matches!(r, IssueReclaimGrantOutcome::Granted(_)), "got {r:?}");
}

// -- reclaim_execution ------------------------------------------------

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_happy_path() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");
    let handle = match r {
        ReclaimExecutionOutcome::Claimed(h) => h,
        other => panic!("expected Claimed, got {other:?}"),
    };
    assert_eq!(handle.kind, HandleKind::Reclaimed);

    let (reclaim_count, attempt_index): (i64, i64) = sqlx::query_as(
        "SELECT lease_reclaim_count, attempt_index FROM ff_exec_core \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(reclaim_count, 1);
    assert_eq!(attempt_index, 1, "new attempt_index = prior (0) + 1");

    let (grant_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant WHERE partition_key=0 \
         AND execution_id=?1 AND kind='reclaim'",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(grant_count, 0, "grant must be deleted after consumption");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_new_attempt_and_prior_marked_interrupted() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");
    let _ = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");

    let (prior_outcome,): (Option<String>,) = sqlx::query_as(
        "SELECT outcome FROM ff_attempt WHERE partition_key=0 \
         AND execution_id=?1 AND attempt_index=0",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(prior_outcome.as_deref(), Some("interrupted_reclaimed"));

    let (new_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_attempt WHERE partition_key=0 \
         AND execution_id=?1 AND attempt_index=1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(new_count, 1, "reclaim must insert a fresh attempt row");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_worker_id_mismatch() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let err = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "other-worker", "other-i1", 0, None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ff_core::engine_error::EngineError::Validation { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_grant_not_found() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");
    assert!(
        matches!(r, ReclaimExecutionOutcome::GrantNotFound { .. }),
        "got {r:?}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_grant_ttl_expired_returns_not_found() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    sqlx::query(
        "UPDATE ff_claim_grant SET expires_at_ms = 1 \
         WHERE partition_key=0 AND execution_id=?1 AND kind='reclaim'",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");
    assert!(
        matches!(r, ReclaimExecutionOutcome::GrantNotFound { .. }),
        "got {r:?}"
    );
    let (grant_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant WHERE partition_key=0 \
         AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(grant_count, 0);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_cap_exceeded() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = 1 \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, Some(1)))
        .await
        .expect("reclaim");
    match r {
        ReclaimExecutionOutcome::ReclaimCapExceeded { reclaim_count, .. } => {
            assert_eq!(reclaim_count, 1);
        }
        other => panic!("expected ReclaimCapExceeded, got {other:?}"),
    }

    let (phase, public_state): (String, String) = sqlx::query_as(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(phase, "terminal");
    assert_eq!(public_state, "failed");
}

/// Investigation: per memory project_reclaim_cap_exceeded_investigation.md,
/// the PR-F (#402) agent observed `lease_reclaim_count=4, max=Some(4)`
/// returning `Claimed` instead of `ReclaimCapExceeded` on Valkey. This
/// SQLite parallel asserts the same exact-boundary scenario enforces
/// the cap here too — the SQLite backend shares none of the Lua code
/// path, but the contract (reclaim_count >= max_reclaim_count → cap
/// exceeded) must be uniform across backends.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_cap_exceeded_at_exact_boundary() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = 4 \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, Some(4)))
        .await
        .expect("reclaim");

    assert!(
        matches!(r, ReclaimExecutionOutcome::ReclaimCapExceeded { .. }),
        "expected ReclaimCapExceeded at lease_reclaim_count=max=4 boundary, got {r:?}"
    );
}

// -- Review-finding regression tests (F1, F2+F3, F4, F5, F7) ---------

/// F1+F6: if the exec transitions to terminal between grant issuance
/// and grant consumption, reclaim_execution returns NotReclaimable
/// (no state mutation) instead of reviving the execution.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_exec_gone_terminal_returns_not_reclaimable() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    // Operator-cancel-style transition flips the exec to terminal
    // after the grant is issued but before the worker consumes it.
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='terminal', ownership_state='unowned', \
         public_state='cancelled' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");
    assert!(
        matches!(r, ReclaimExecutionOutcome::NotReclaimable { .. }),
        "got {r:?}"
    );

    // Critical: exec state must still be terminal (not revived).
    let (phase,): (String,) = sqlx::query_as(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(phase, "terminal", "exec must remain terminal, not revived");
}

/// F2+F3: cap-exceeded branch clears the prior attempt's lease
/// fields, marks outcome = interrupted_reclaimed, and emits BOTH
/// the completion_event (terminal_failed) and the lease_event
/// (revoked) per RFC-019 §4.2.7 outbox matrix.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_cap_exceeded_clears_prior_attempt_and_emits_events() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = 1 \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, Some(1)))
        .await
        .expect("reclaim");
    assert!(matches!(r, ReclaimExecutionOutcome::ReclaimCapExceeded { .. }));

    // Prior attempt lease-fencing fields cleared + outcome set.
    let (outcome, worker_id, lease_expires): (Option<String>, Option<String>, Option<i64>) =
        sqlx::query_as(
            "SELECT outcome, worker_id, lease_expires_at_ms FROM ff_attempt \
             WHERE partition_key=0 AND execution_id=?1 AND attempt_index=0",
        )
        .bind(exec_uuid)
        .fetch_one(b.pool_for_test())
        .await
        .unwrap();
    assert_eq!(outcome.as_deref(), Some("interrupted_reclaimed"));
    assert!(worker_id.is_none(), "worker_id must be cleared");
    assert!(lease_expires.is_none(), "lease_expires_at_ms must be cleared");

    // completion_event emitted with outcome = 'failed'.
    let (comp_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_completion_event WHERE partition_key=0 \
         AND execution_id=?1 AND outcome='failed'",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(comp_count, 1, "terminal_failed must emit completion_event");

    // lease_event emitted with event_type = 'revoked'. Column is
    // TEXT, bind the hyphenated UUID string.
    let (lease_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_lease_event WHERE partition_key=0 \
         AND execution_id=?1 AND event_type='revoked'",
    )
    .bind(exec_uuid.to_string())
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(lease_count, 1, "terminal_failed must emit lease_event revoked");
}

/// F4: the minted handle embeds caller-supplied `attempt_id` +
/// `lease_id` (not freshly minted internally). Also verifies
/// LeaseEpoch is non-zero (F5 — monotonically bumped).
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_handle_carries_caller_ids_and_nonzero_epoch() {
    use ff_core::handle_codec::decode as decode_handle;

    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    let caller_attempt_id = AttemptId::new();
    let caller_lease_id = LeaseId::new();
    let args = ReclaimExecutionArgs::new(
        exec_id.clone(),
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i2"),
        lane_id.clone(),
        None,
        caller_lease_id.clone(),
        30_000,
        caller_attempt_id.clone(),
        String::new(),
        None,
        WorkerInstanceId::new("w1-i1"),
        AttemptIndex::new(0),
    );
    let r = b.reclaim_execution(args).await.expect("reclaim");
    let handle = match r {
        ReclaimExecutionOutcome::Claimed(h) => h,
        other => panic!("expected Claimed, got {other:?}"),
    };

    let decoded = decode_handle(&handle.opaque).expect("decode handle");
    assert_eq!(decoded.payload.attempt_id, caller_attempt_id, "attempt_id must round-trip");
    assert_eq!(decoded.payload.lease_id, caller_lease_id, "lease_id must round-trip");
    assert!(
        decoded.payload.lease_epoch.0 >= 1,
        "lease_epoch must be monotonically bumped (>=1), got {}",
        decoded.payload.lease_epoch.0
    );
}

/// F5: lease_epoch monotonically increases across successive
/// reclaims. Each reclaim reads the prior attempt's epoch and writes
/// prior + 1 to the new attempt row.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_lease_epoch_monotonic_across_reclaims() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);

    // First reclaim (attempt_index 0 → 1).
    force_lease_expired(&b, exec_uuid).await;
    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue-1");
    let _ = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim-1");

    let (epoch_1,): (i64,) = sqlx::query_as(
        "SELECT lease_epoch FROM ff_attempt WHERE partition_key=0 \
         AND execution_id=?1 AND attempt_index=1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert!(epoch_1 >= 1, "first reclaim epoch must be >=1, got {epoch_1}");

    // Second reclaim (1 → 2).
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='lease_expired_reclaimable' \
         WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue-2");
    let _ = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i3", 1, None))
        .await
        .expect("reclaim-2");

    let (epoch_2,): (i64,) = sqlx::query_as(
        "SELECT lease_epoch FROM ff_attempt WHERE partition_key=0 \
         AND execution_id=?1 AND attempt_index=2",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert!(
        epoch_2 > epoch_1,
        "second-reclaim epoch ({epoch_2}) must exceed first-reclaim epoch ({epoch_1})"
    );
}

/// F7: caller-supplied `current_attempt_index` is ignored in favor of
/// the server-authoritative `ff_exec_core.attempt_index`. A stale arg
/// does not backward-pin state nor PK-collide.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_ignores_stale_current_attempt_index_arg() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");

    // Stale arg: caller thinks attempt_index is 999, but server
    // authoritative is 0. Server must derive new_attempt_index = 0+1.
    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 999, None))
        .await
        .expect("reclaim");
    assert!(matches!(r, ReclaimExecutionOutcome::Claimed(_)), "got {r:?}");

    let (attempt_index,): (i64,) = sqlx::query_as(
        "SELECT attempt_index FROM ff_exec_core WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .fetch_one(b.pool_for_test())
    .await
    .unwrap();
    assert_eq!(
        attempt_index, 1,
        "server-derived attempt_index must be 1 (stored 0 + 1), NOT 1000 (stale arg + 1)"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn reclaim_execution_handle_kind_reclaimed() {
    let b = fresh_backend().await;
    let (exec_id, lane_id, _handle) = create_and_claim(&b).await;
    let exec_uuid = uuid_of(&exec_id);
    force_lease_expired(&b, exec_uuid).await;

    let _ = b
        .issue_reclaim_grant(issue_args(&exec_id, &lane_id))
        .await
        .expect("issue");
    let r = b
        .reclaim_execution(reclaim_args(&exec_id, &lane_id, "w1", "w1-i2", 0, None))
        .await
        .expect("reclaim");
    match r {
        ReclaimExecutionOutcome::Claimed(h) => {
            assert_eq!(h.kind, HandleKind::Reclaimed);
        }
        other => panic!("expected Claimed(Reclaimed), got {other:?}"),
    }
}
