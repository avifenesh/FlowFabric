//! Integration coverage for
//! [`ff_core::engine_backend::EngineBackend::claim`] and
//! [`ff_core::engine_backend::EngineBackend::claim_from_resume_grant`] on
//! the Valkey-backed impl (v0.7 Wave 2).
//!
//! Covers:
//!   * Fresh-claim happy path — scan lands on the seeded partition,
//!     issues a grant, claims via `ff_claim_execution`, returns a
//!     `HandleKind::Fresh` handle.
//!   * Capability-subset mismatch — eligible candidate exists but the
//!     worker's CapabilitySet doesn't cover required_capabilities, so
//!     `claim` returns `Ok(None)`.
//!   * Empty eligible set — nothing to claim, `Ok(None)`.
//!   * claim_from_resume_grant happy path — consumes a `ResumeToken`
//!     (Wave 2 extended shape with worker identity) and mints a
//!     `HandleKind::Resumed` handle with a bumped lease_epoch.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_claim -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{
    BackendTag, CapabilitySet, ClaimPolicy, HandleKind, ResumeToken,
};
use ff_core::contracts::{DeliverSignalArgs, ResumeGrant};
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig, PartitionKey};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "ebc-lane";
const NS: &str = "ebc-ns";
const WORKER: &str = "ebc-worker";
const WORKER_INST: &str = "ebc-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), config())
}

/// Create an execution and return its id. If `required_caps_csv` is
/// non-empty, the exec's `policy_json.routing_requirements.required_capabilities`
/// is populated.
async fn create_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    required_caps_csv: &str,
) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let policy_json = if required_caps_csv.is_empty() {
        "{}".to_owned()
    } else {
        let caps: Vec<&str> = required_caps_csv.split(',').collect();
        serde_json::json!({
            "routing_requirements": { "required_capabilities": caps }
        })
        .to_string()
    };

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "ebc".to_owned(),
        "0".to_owned(),
        "ebc-runner".to_owned(),
        policy_json,
        r#"{"test":true}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("ff_create_execution");
}

async fn issue_grant(tc: &TestCluster, eid: &ExecutionId, caps_csv: &str) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let grant_keys: Vec<String> =
        vec![ctx.core(), ctx.claim_grant(), idx.lane_eligible(&lane_id)];
    let grant_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        caps_csv.to_owned(),
    ];
    let kr: Vec<&str> = grant_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = grant_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("ff_issue_claim_grant");
}

fn base_policy(lease_ttl_ms: u32) -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        lease_ttl_ms,
        None,
    )
}

#[tokio::test]
#[serial_test::serial]
async fn claim_happy_path_returns_fresh_handle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    create_execution(&tc, &eid, "").await;

    let backend = build_backend(&tc).await;
    let lane = LaneId::new(LANE);
    let caps = CapabilitySet::new::<_, String>(Vec::<String>::new());
    let policy = base_policy(30_000);

    let handle = backend
        .claim(&lane, &caps, policy)
        .await
        .expect("claim should succeed on a seeded eligible exec")
        .expect("claim should return Some on a matching exec");

    assert_eq!(handle.backend, BackendTag::Valkey);
    assert_eq!(handle.kind, HandleKind::Fresh);
}

#[tokio::test]
#[serial_test::serial]
async fn claim_empty_eligible_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let backend = build_backend(&tc).await;
    let lane = LaneId::new(LANE);
    let caps = CapabilitySet::new::<_, String>(Vec::<String>::new());
    let policy = base_policy(30_000);

    let out = backend
        .claim(&lane, &caps, policy)
        .await
        .expect("claim on empty lane should be Ok(None)");
    assert!(out.is_none(), "expected Ok(None), got {out:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn claim_capability_mismatch_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    create_execution(&tc, &eid, "gpu").await;

    let backend = build_backend(&tc).await;
    let lane = LaneId::new(LANE);
    // Worker advertises no caps; exec requires "gpu" — must skip.
    let caps = CapabilitySet::new::<_, String>(Vec::<String>::new());
    let policy = base_policy(30_000);

    let out = backend
        .claim(&lane, &caps, policy)
        .await
        .expect("claim with mismatched caps should be Ok(None)");
    assert!(out.is_none(), "expected Ok(None), got {out:?}");
}

// ───────────────────────── claim_from_resume_grant ─────────────────────────

/// Seed an execution, claim it, suspend it, deliver a resuming signal,
/// and leave the exec ready for `claim_from_resume_grant`. Returns
/// `(exec_id, lease_epoch_before_reclaim)`.
async fn seed_resumable(tc: &TestCluster, eid: &ExecutionId) -> u64 {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    create_execution(tc, eid, "").await;
    issue_grant(tc, eid, "").await;

    // Claim via raw FCALL so we keep the lease_id + attempt_id for the
    // suspend ARGV below.
    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let att_idx = AttemptIndex::new(0);
    let lease_ttl_ms: u64 = 30_000;
    let renew_before = lease_ttl_ms * 2 / 3;
    let claim_keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.attempt_hash(att_idx),
        ctx.attempt_usage(att_idx),
        ctx.attempt_policy(att_idx),
        ctx.attempts(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];
    let claim_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        lease_id.clone(),
        lease_ttl_ms.to_string(),
        renew_before.to_string(),
        attempt_id.clone(),
        "{}".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = claim_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = claim_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_claim_execution", &kr, &ar)
        .await
        .expect("ff_claim_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("claim: expected Array"),
    };
    let epoch_str = match arr.get(3) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => panic!("claim: no epoch"),
    };
    let epoch_before: u64 = epoch_str.parse().expect("epoch parse");

    // Suspend and collect a waitpoint token.
    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk:{wp_id_str}");

    let suspend_keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        idx.suspension_timeout(),
        idx.pending_waitpoint_expiry(),
        idx.lane_active(&lane_id),
        idx.lane_suspended(&lane_id),
        ctx.waitpoints(),
        ctx.waitpoint_condition(&wp_id),
        idx.attempt_timeout(),
        idx.waitpoint_hmac_secrets(),
    ];
    let resume_condition_json = serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": ["ebc_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": 1,
        "timeout_behavior": "fail",
        "allow_operator_override": true,
    })
    .to_string();
    let resume_policy_json = serde_json::json!({
        "resume_target": "runnable",
        "close_waitpoint_on_resume": true,
        "consume_matched_signals": true,
        "retain_signal_buffer_until_closed": true,
    })
    .to_string();
    let suspend_args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        attempt_id.clone(),
        lease_id.clone(),
        epoch_str.clone(),
        uuid::Uuid::new_v4().to_string(),
        wp_id_str.clone(),
        wp_key,
        "waiting_for_signal".to_owned(),
        "worker".to_owned(),
        String::new(),
        resume_condition_json,
        resume_policy_json,
        String::new(),
        "0".to_owned(),
        "fail".to_owned(),
        "1000".to_owned(),
    ];
    let kr: Vec<&str> = suspend_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = suspend_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_suspend_execution", &kr, &ar)
        .await
        .expect("ff_suspend_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("suspend: expected Array"),
    };
    let token = match arr.get(5) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("suspend: missing token"),
    };

    // Deliver the resuming signal via the backend (flips the execution
    // to `runnable + attempt_interrupted`).
    let backend = build_backend(tc).await;
    let sig_args = DeliverSignalArgs {
        execution_id: eid.clone(),
        waitpoint_id: wp_id,
        signal_id: SignalId::new(),
        signal_name: "ebc_signal".to_owned(),
        signal_category: "test".to_owned(),
        source_type: "external".to_owned(),
        source_identity: "ebc".to_owned(),
        payload: None,
        payload_encoding: Some("json".to_owned()),
        correlation_id: None,
        idempotency_key: None,
        target_scope: "waitpoint".to_owned(),
        created_at: Some(TimestampMs::now()),
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: WaitpointToken::new(token),
        now: TimestampMs::now(),
    };
    backend
        .deliver_signal(sig_args)
        .await
        .expect("deliver_signal should resume");

    // A fresh grant is required before claim_from_resume_grant consumes it.
    issue_grant(tc, eid, "").await;

    epoch_before
}

fn synthetic_reclaim_grant(eid: &ExecutionId) -> ResumeGrant {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    ResumeGrant::new(
        eid.clone(),
        PartitionKey::from(&partition),
        ctx.claim_grant(),
        0, // unused by the Valkey resume path
        LaneId::new(LANE),
    )
}

#[tokio::test]
#[serial_test::serial]
async fn claim_from_reclaim_happy_path_bumps_epoch() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let epoch_before = seed_resumable(&tc, &eid).await;

    let grant = synthetic_reclaim_grant(&eid);
    let token = ResumeToken::new(
        grant,
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        30_000,
    );

    let backend = build_backend(&tc).await;
    let handle = backend
        .claim_from_resume_grant(token)
        .await
        .expect("claim_from_resume_grant should succeed")
        .expect("claim_from_resume_grant should return Some on resumed exec");

    assert_eq!(handle.backend, BackendTag::Valkey);
    assert_eq!(handle.kind, HandleKind::Resumed);

    // Lease epoch must have advanced (RFC-003 — each lease transition
    // monotonically bumps the epoch).
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let epoch_after_str: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(ctx.lease_current())
        .arg("lease_epoch")
        .execute()
        .await
        .expect("HGET lease_epoch");
    let epoch_after: u64 = epoch_after_str
        .as_deref()
        .and_then(|s| s.parse().ok())
        .expect("lease_epoch must be present after resume claim");
    assert!(
        epoch_after > epoch_before,
        "expected bumped lease_epoch, before={epoch_before} after={epoch_after}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn claim_from_reclaim_double_consume_surfaces_typed_error() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let _ = seed_resumable(&tc, &eid).await;

    let grant = synthetic_reclaim_grant(&eid);
    let token = ResumeToken::new(
        grant,
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        30_000,
    );

    let backend = build_backend(&tc).await;
    let _first = backend
        .claim_from_resume_grant(token.clone())
        .await
        .expect("first claim_from_resume_grant should succeed")
        .expect("first claim_from_resume_grant should return Some");

    // Second consume on the same grant: the grant key was consumed by
    // the first FCALL and the lease is now held by the first claim, so
    // the FCALL surfaces a typed ScriptError (invalid_claim_grant /
    // claim_grant_consumed / lease_conflict depending on wire state).
    let err = backend
        .claim_from_resume_grant(token)
        .await
        .expect_err("second claim_from_resume_grant should fail");
    let s = err.to_string();
    assert!(
        s.contains("invalid_claim_grant")
            || s.contains("claim_grant")
            || s.contains("lease_conflict")
            || s.contains("LeaseConflict")
            || s.contains("not_a_resumed_execution")
            || s.contains("stale_lease")
            || s.contains("ExecutionNotLeaseable")
            || s.contains("execution_not_leaseable"),
        "expected typed conflict error, got: {s}"
    );
}
