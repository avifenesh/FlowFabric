//! Integration coverage for cairn #322 —
//! [`ff_core::engine_backend::EngineBackend::suspend_by_triple`] on the
//! Valkey-backed impl.
//!
//! Mirrors the happy-path flow of `engine_backend_suspend.rs` but
//! drives suspend via the service-layer entry point (exec_id + lease
//! fence triple) instead of a Handle. Asserts the returned outcome is
//! equivalent to `suspend(&Handle, ..)` and that the suspended handle
//! on success is tagged `HandleKind::Suspended`.

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{BackendTag, HandleKind};
use ff_core::contracts::{
    ResumeCondition, ResumePolicy, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "sbt-lane";
const NS: &str = "sbt-ns";
const WORKER: &str = "sbt-worker";
const WORKER_INST: &str = "sbt-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), config())
}

/// Seed a live execution + claim and return the fence triple the
/// caller would have captured from the claim record.
async fn create_and_claim_triple(tc: &TestCluster, eid: &ExecutionId) -> LeaseFence {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

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
        "sbt".to_owned(),
        "0".to_owned(),
        "sbt-runner".to_owned(),
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

    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
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
        lease_id.to_string(),
        lease_ttl_ms.to_string(),
        renew_before.to_string(),
        attempt_id.to_string(),
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
    let lease_epoch = LeaseEpoch(epoch_str.parse::<u64>().unwrap_or(0));

    LeaseFence {
        lease_id,
        lease_epoch,
        attempt_id,
    }
}

fn make_single_args(wp_key: &str, matcher: &str) -> SuspendArgs {
    SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: wp_key.to_owned(),
        },
        ResumeCondition::Single {
            waitpoint_key: wp_key.to_owned(),
            matcher: SignalMatcher::ByName(matcher.to_owned()),
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
}

#[tokio::test]
#[serial_test::serial]
async fn suspend_by_triple_fresh_returns_suspended_handle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let triple = create_and_claim_triple(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let args = make_single_args(&wp_key, "approve");
    let suspension_id = args.suspension_id.clone();

    let outcome = backend
        .suspend_by_triple(eid.clone(), triple, args)
        .await
        .expect("suspend_by_triple: fresh → Suspended");

    match outcome {
        SuspendOutcome::Suspended { details, handle: h } => {
            assert_eq!(details.suspension_id, suspension_id);
            assert_eq!(details.waitpoint_key, wp_key);
            assert_eq!(h.kind, HandleKind::Suspended);
            assert_eq!(h.backend, BackendTag::Valkey);
            assert!(!details.waitpoint_token.as_str().is_empty());
        }
        other => panic!("expected Suspended, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn suspend_by_triple_stale_fence_surfaces_contention() {
    // Triple with a bumped lease_epoch must fail the Lua fence check
    // and surface as a typed error — same contract as `suspend`.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let mut triple = create_and_claim_triple(&tc, &eid).await;
    triple.lease_epoch = LeaseEpoch(triple.lease_epoch.0 + 999);
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let args = make_single_args(&wp_key, "approve");

    let err = backend
        .suspend_by_triple(eid, triple, args)
        .await
        .expect_err("stale lease_epoch must reject");
    // The Lua fence path surfaces as a typed State / Contention /
    // Validation error; the exact discriminant is decided by ff-script.
    // We just assert the call did not silently succeed and that the
    // underlying reason is stale-lease or fence-related.
    let s = err.to_string();
    assert!(
        s.to_lowercase().contains("stale")
            || s.to_lowercase().contains("lease")
            || s.to_lowercase().contains("fence")
            || matches!(
                err,
                ff_core::engine_error::EngineError::State(_)
                    | ff_core::engine_error::EngineError::Contention(_)
                    | ff_core::engine_error::EngineError::Validation { .. }
            ),
        "unexpected error shape: {s}"
    );
}
