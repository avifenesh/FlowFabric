//! Integration coverage for
//! [`ff_core::engine_backend::EngineBackend::deliver_signal`] on the
//! Valkey-backed impl (issue #150).
//!
//! Seeds a real execution + suspension via the canonical FCALL path
//! (so the waitpoint, HMAC secret, and resume condition all match the
//! on-wire shape), then fires the trait method with the minted
//! waitpoint token and asserts:
//!   - happy path: returns [`DeliverSignalResult::Accepted`] with a
//!     `resume_condition_satisfied` effect
//!   - idempotent replay: same idempotency_key → `Duplicate`
//!   - missing token: surfaces as a typed transport-wrapped
//!     `ScriptError` (invalid_token / missing_token)
//!
//! Seeding mirrors `waitpoint_tokens.rs` because that is the only
//! path that exercises the same atomic state transitions
//! `ff_deliver_signal` validates against.
//!
//! Run with: cargo test -p ff-test --test engine_backend_deliver_signal -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::contracts::{DeliverSignalArgs, DeliverSignalResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "ebds-lane";
const NS: &str = "ebds-ns";
const WORKER: &str = "ebds-worker";
const WORKER_INST: &str = "ebds-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), config())
}

async fn create_and_claim(tc: &TestCluster, eid: &ExecutionId) -> (String, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    // 1. create_execution
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
        "ebds".to_owned(),
        "0".to_owned(),
        "ebds-runner".to_owned(),
        "{}".to_owned(),
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

    // 2. issue_claim_grant
    let grant_keys: Vec<String> = vec![ctx.core(), ctx.claim_grant(), idx.lane_eligible(&lane_id)];
    let grant_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = grant_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = grant_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("ff_issue_claim_grant");

    // 3. claim_execution
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
    let epoch = match arr.get(3) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => panic!("claim: no epoch"),
    };
    (lease_id, epoch, attempt_id)
}

async fn suspend_and_get_token_with_min_count(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
    minimum_signal_count: u32,
) -> (WaitpointId, WaitpointToken) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk:{wp_id_str}");

    let keys: Vec<String> = vec![
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
        "required_signal_names": ["ebds_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": minimum_signal_count,
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

    let args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        attempt_id.to_owned(),
        lease_id.to_owned(),
        epoch.to_owned(),
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
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
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
        _ => panic!("suspend: missing waitpoint_token: {raw:?}"),
    };
    (wp_id, WaitpointToken::new(token))
}

async fn suspend_and_get_token(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
) -> (WaitpointId, WaitpointToken) {
    suspend_and_get_token_with_min_count(tc, eid, lease_id, epoch, attempt_id, 1).await
}

fn build_args(
    eid: &ExecutionId,
    wp_id: &WaitpointId,
    token: &WaitpointToken,
    signal_name: &str,
    idempotency_key: Option<String>,
) -> DeliverSignalArgs {
    DeliverSignalArgs {
        execution_id: eid.clone(),
        waitpoint_id: wp_id.clone(),
        signal_id: SignalId::new(),
        signal_name: signal_name.to_owned(),
        signal_category: "test".to_owned(),
        source_type: "external".to_owned(),
        source_identity: "ebds".to_owned(),
        payload: None,
        payload_encoding: Some("json".to_owned()),
        correlation_id: None,
        idempotency_key,
        target_scope: "waitpoint".to_owned(),
        created_at: Some(TimestampMs::now()),
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: token.clone(),
        now: TimestampMs::now(),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn deliver_signal_happy_path_resumes_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, token) = suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let backend = build_backend(&tc).await;
    let args = build_args(&eid, &wp_id, &token, "ebds_signal", None);

    let result = backend
        .deliver_signal(args)
        .await
        .expect("deliver_signal should succeed with valid token");

    match result {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(
                effect, "resume_condition_satisfied",
                "single-signal condition should resume on first delivery"
            );
        }
        other => panic!("expected Accepted, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn deliver_signal_idempotent_replay_returns_duplicate() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = create_and_claim(&tc, &eid).await;
    // minimum_signal_count=2 so the first signal just appends to the
    // waitpoint without closing it; the second delivery with the same
    // idempotency_key then hits the dedup branch rather than a
    // closed-waitpoint reject.
    let (wp_id, token) =
        suspend_and_get_token_with_min_count(&tc, &eid, &lease_id, &epoch, &attempt_id, 2).await;

    let backend = build_backend(&tc).await;
    let idem = Some("dedup-key-1".to_owned());
    let args1 = build_args(&eid, &wp_id, &token, "ebds_signal", idem.clone());
    let first = backend.deliver_signal(args1).await.expect("first deliver");
    let first_sig_id = match first {
        DeliverSignalResult::Accepted { signal_id, .. } => signal_id,
        other => panic!("expected Accepted, got {other:?}"),
    };

    let args2 = build_args(&eid, &wp_id, &token, "ebds_signal", idem);
    let second = backend
        .deliver_signal(args2)
        .await
        .expect("second deliver (idempotent replay)");
    match second {
        DeliverSignalResult::Duplicate { existing_signal_id } => {
            assert_eq!(
                existing_signal_id, first_sig_id,
                "Duplicate should echo the first signal_id"
            );
        }
        other => panic!("expected Duplicate, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn deliver_signal_missing_token_surfaces_error() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _token) = suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let backend = build_backend(&tc).await;
    let empty_token = WaitpointToken::new(String::new());
    let args = build_args(&eid, &wp_id, &empty_token, "ebds_signal", None);

    let err = backend
        .deliver_signal(args)
        .await
        .expect_err("empty token should be rejected");

    // Surfaces as a Transport-wrapped ScriptError::InvalidToken /
    // MissingToken on the Valkey backend. The trait contract only
    // promises typed error; don't depend on the exact downcast.
    let s = err.to_string();
    assert!(
        s.contains("missing_token")
            || s.contains("invalid_token")
            || s.contains("deliver_signal"),
        "expected token-rejection error, got: {s}"
    );
}
