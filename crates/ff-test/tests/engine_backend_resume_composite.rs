//! Integration coverage for RFC-014 multi-signal resume conditions.
//!
//! Exercises the `AllOf` / `Count{DistinctWaitpoints,DistinctSignals,
//! DistinctSources}` composite body through the
//! [`EngineBackend::suspend`] + [`EngineBackend::deliver_signal`] trait
//! surface. Covers RFC-014 patterns 1 + 2 (shared-waitpoint composites)
//! and pattern 3 (heterogeneous subsystems with N distinct waitpoints,
//! widened `SuspendArgs.waitpoints: Vec<WaitpointBinding>`).
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_resume_composite -- --test-threads=1
//!
//! Test matrix (RFC-014 §10.2):
//!   - allof_two_matchers_resumes_when_both_fired
//!   - allof_duplicate_signal_idempotent
//!   - count_distinct_waitpoints_resumes_on_threshold
//!   - count_distinct_signals_counts_by_signal_id
//!   - count_distinct_signals_duplicate_signal_id_dedups
//!   - count_distinct_sources_counts_by_source
//!   - count_distinct_sources_ignores_duplicate_source
//!   - count_matcher_filters_non_matching_signals
//!   - allof_depth_4_nested_accepted
//!   - allof_depth_5_rejected_invalid_condition
//!   - count_n_zero_rejected_invalid_condition
//!   - count_waitpoints_empty_rejected_invalid_condition
//!   - resume_payload_exposes_all_satisfier_signals
//!   - cancel_deletes_satisfied_set_and_member_map
//!   - expire_deletes_satisfied_set_and_member_map
//!   - allof_three_waitpoints_resumes_when_all_fired  (Pattern 3)
//!   - multi_waitpoint_keys_colocated_under_partition_hash_tag (Pattern 3)
//!   - cancel_closes_every_waitpoint_in_multi_binding (Pattern 3)

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque};
use ff_core::contracts::{
    CompositeBody, CountKind, DeliverSignalArgs, DeliverSignalResult, ResumeCondition,
    ResumePolicy, SignalMatcher, SuspendArgs, SuspendOutcome, SuspensionReasonCode,
    TimeoutBehavior, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "ebrc-lane";
const NS: &str = "ebrc-ns";
const WORKER: &str = "ebrc-worker";
const WORKER_INST: &str = "ebrc-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), config())
}

async fn create_and_claim_handle(tc: &TestCluster, eid: &ExecutionId) -> Handle {
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
        "ebrc".to_owned(),
        "0".to_owned(),
        "ebrc-runner".to_owned(),
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

    ValkeyBackend::encode_handle(
        eid.clone(),
        att_idx,
        attempt_id,
        lease_id,
        lease_epoch,
        lease_ttl_ms,
        lane_id,
        wid,
        HandleKind::Fresh,
    )
}

fn composite_args(wp_key: &str, body: CompositeBody) -> SuspendArgs {
    SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: wp_key.to_owned(),
        },
        ResumeCondition::Composite(body),
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_timeout(
        TimestampMs::from_millis(TimestampMs::now().0 + 60_000),
        TimeoutBehavior::Fail,
    )
}

fn deliver_args(
    eid: &ExecutionId,
    wp_id: WaitpointId,
    signal_name: &str,
    source_identity: &str,
    token: WaitpointToken,
) -> DeliverSignalArgs {
    DeliverSignalArgs {
        execution_id: eid.clone(),
        waitpoint_id: wp_id,
        signal_id: SignalId::new(),
        signal_name: signal_name.to_owned(),
        signal_category: "test".to_owned(),
        source_type: "user".to_owned(),
        source_identity: source_identity.to_owned(),
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
        waitpoint_token: token,
        now: TimestampMs::now(),
    }
}

fn single_byname(wp_key: &str, name: &str) -> ResumeCondition {
    ResumeCondition::Single {
        waitpoint_key: wp_key.to_owned(),
        matcher: SignalMatcher::ByName(name.to_owned()),
    }
}

// ── AllOf happy-path + idempotency ──────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn allof_two_matchers_resumes_when_both_fired() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::AllOf {
        members: vec![
            single_byname(&wp_key, "reviewer_a"),
            single_byname(&wp_key, "reviewer_b"),
        ],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .expect("suspend AllOf");
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("expected Suspended, got {other:?}"),
    };

    // First signal: reviewer_a — suspension stays open.
    let r1 = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "reviewer_a", "a", token.clone()))
        .await
        .unwrap();
    match r1 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint", "first of two AllOf matchers");
        }
        other => panic!("expected Accepted, got {other:?}"),
    }

    // Second signal: reviewer_b — resume.
    let r2 = backend
        .deliver_signal(deliver_args(&eid, wp_id, "reviewer_b", "b", token))
        .await
        .unwrap();
    match r2 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected Accepted, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn allof_duplicate_signal_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::AllOf {
        members: vec![
            single_byname(&wp_key, "alpha"),
            single_byname(&wp_key, "beta"),
        ],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "alpha", "a", token.clone()))
        .await
        .unwrap();
    // Duplicate alpha — should dedup at the token layer.
    let dup = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "alpha", "a2", token.clone()))
        .await
        .unwrap();
    match dup {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint_duplicate");
        }
        other => panic!("expected duplicate Accepted, got {other:?}"),
    }
    // Finishing beta still resumes.
    let fin = backend
        .deliver_signal(deliver_args(&eid, wp_id, "beta", "b", token))
        .await
        .unwrap();
    match fin {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected resume, got {other:?}"),
    }
}

// ── Count{DistinctWaitpoints} ─────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn count_distinct_waitpoints_resumes_on_threshold() {
    // Single-waitpoint scope: n=1 against a single-key waitpoint set.
    // Covers the DistinctWaitpoints kind serialization + evaluation
    // without requiring multi-binding SuspendArgs.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 1,
        count_kind: CountKind::DistinctWaitpoints,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };
    let r = backend
        .deliver_signal(deliver_args(&eid, wp_id, "go", "u1", token))
        .await
        .unwrap();
    match r {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected resume, got {other:?}"),
    }
}

// ── Count{DistinctSignals} ────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn count_distinct_signals_counts_by_signal_id() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSignals,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    let r1 = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "callback", "src", token.clone()))
        .await
        .unwrap();
    assert!(matches!(r1, DeliverSignalResult::Accepted { ref effect, .. } if effect == "appended_to_waitpoint"));
    let r2 = backend
        .deliver_signal(deliver_args(&eid, wp_id, "callback", "src", token))
        .await
        .unwrap();
    match r2 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected resume, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn count_distinct_signals_duplicate_signal_id_dedups() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSignals,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    // Deliver with an explicit idempotency key and re-deliver the
    // same idempotency key; RFC-005 signal-level dedup + RFC-014
    // token-level dedup both fire.
    let mut a1 = deliver_args(&eid, wp_id.clone(), "callback", "src", token.clone());
    a1.idempotency_key = Some("idem-1".into());
    let _ = backend.deliver_signal(a1.clone()).await.unwrap();
    let dup = backend.deliver_signal(a1).await.unwrap();
    // Signal-level dedup returns Duplicate.
    match dup {
        DeliverSignalResult::Duplicate { .. } => {}
        other => panic!("expected Duplicate from signal-level dedup, got {other:?}"),
    }
}

// ── Count{DistinctSources} ────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn count_distinct_sources_counts_by_source() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "approve", "alice", token.clone()))
        .await
        .unwrap();
    let r = backend
        .deliver_signal(deliver_args(&eid, wp_id, "approve", "bob", token))
        .await
        .unwrap();
    match r {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected resume, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn count_distinct_sources_ignores_duplicate_source() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "approve", "alice", token.clone()))
        .await
        .unwrap();
    // Same source_identity again — token dedup.
    let dup = backend
        .deliver_signal(deliver_args(&eid, wp_id, "approve", "alice", token))
        .await
        .unwrap();
    match dup {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint_duplicate");
        }
        other => panic!("expected duplicate, got {other:?}"),
    }
}

// ── Matcher filter (RFC §3.3 step 2.5) ────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn count_matcher_filters_non_matching_signals() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 1,
        count_kind: CountKind::DistinctSignals,
        matcher: Some(SignalMatcher::ByName("approve".into())),
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };

    // A signal with a different name must NOT satisfy the node.
    let r = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "reject", "u", token.clone()))
        .await
        .unwrap();
    match r {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "signal_ignored_matcher_failed");
        }
        other => panic!("expected matcher-failed, got {other:?}"),
    }
    // Matching name satisfies.
    let r2 = backend
        .deliver_signal(deliver_args(&eid, wp_id, "approve", "u", token))
        .await
        .unwrap();
    match r2 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected resume, got {other:?}"),
    }
}

// ── Depth caps (pure Rust validation) ─────────────────────────────

#[tokio::test]
async fn allof_depth_4_nested_accepted() {
    let leaf = single_byname("wpk:leaf", "x");
    let d4 = ResumeCondition::Composite(CompositeBody::AllOf {
        members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
            members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                    members: vec![leaf],
                })],
            })],
        })],
    });
    assert!(d4.validate_composite().is_ok());
}

#[tokio::test]
async fn allof_depth_5_rejected_invalid_condition() {
    let tc = TestCluster::connect().await;
    let backend = build_backend(&tc).await;

    let handle = Handle::new(
        BackendTag::Valkey,
        HandleKind::Fresh,
        HandleOpaque::new(Box::new([])),
    );
    let leaf = single_byname("wpk:leaf", "x");
    let d5 = ResumeCondition::Composite(CompositeBody::AllOf {
        members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
            members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                    members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                        members: vec![leaf],
                    })],
                })],
            })],
        })],
    });
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: "wpk:x".into(),
        },
        d5,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let err = backend.suspend(&handle, args).await.expect_err("depth 5 must reject");
    match err {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert!(detail.contains("exceeds cap"), "detail: {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn count_n_zero_rejected_invalid_condition() {
    let tc = TestCluster::connect().await;
    let backend = build_backend(&tc).await;
    let handle = Handle::new(
        BackendTag::Valkey,
        HandleKind::Fresh,
        HandleOpaque::new(Box::new([])),
    );
    let body = CompositeBody::Count {
        n: 0,
        count_kind: CountKind::DistinctSignals,
        matcher: None,
        waitpoints: vec!["wpk:x".into()],
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: "wpk:x".into(),
        },
        ResumeCondition::Composite(body),
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let err = backend.suspend(&handle, args).await.expect_err("n=0 must reject");
    match err {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert!(detail.contains("count_n_zero"), "detail: {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn count_waitpoints_empty_rejected_invalid_condition() {
    let tc = TestCluster::connect().await;
    let backend = build_backend(&tc).await;
    let handle = Handle::new(
        BackendTag::Valkey,
        HandleKind::Fresh,
        HandleOpaque::new(Box::new([])),
    );
    let body = CompositeBody::Count {
        n: 1,
        count_kind: CountKind::DistinctSignals,
        matcher: None,
        waitpoints: vec![],
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: "wpk:x".into(),
        },
        ResumeCondition::Composite(body),
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let err = backend
        .suspend(&handle, args)
        .await
        .expect_err("empty waitpoints must reject");
    match err {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert!(
                detail.contains("count_waitpoints_empty"),
                "detail: {detail}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ── Resume payload carries all satisfier signal ids ───────────────

#[tokio::test]
#[serial_test::serial]
async fn resume_payload_exposes_all_satisfier_signals() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };
    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "approve", "alice", token.clone()))
        .await
        .unwrap();
    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id, "approve", "bob", token))
        .await
        .unwrap();

    // Read suspension_current back and assert closer_signal_id +
    // all_satisfier_signals were populated.
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let key = ctx.suspension_current();
    let raw_all: Value = tc
        .client()
        .cmd("HGET")
        .arg(key.as_str())
        .arg("all_satisfier_signals")
        .execute()
        .await
        .expect("HGET all_satisfier_signals");
    let all_sigs = match raw_all {
        Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
        Value::SimpleString(s) => s,
        Value::Nil => panic!("all_satisfier_signals field missing after composite resume"),
        other => panic!("unexpected HGET value: {other:?}"),
    };
    assert!(
        all_sigs.starts_with('[') && all_sigs.len() > 2,
        "all_satisfier_signals should be a non-empty JSON array, got {all_sigs}"
    );
}

// ── Cleanup on cancel + expire (§3.1.1) ───────────────────────────

async fn key_exists(tc: &TestCluster, key: &str) -> bool {
    let v: Value = tc
        .client()
        .cmd("EXISTS")
        .arg(key)
        .execute()
        .await
        .expect("EXISTS");
    matches!(v, Value::Int(1))
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_deletes_satisfied_set_and_member_map() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let outcome = backend
        .suspend(&handle, composite_args(&wp_key, body))
        .await
        .unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };
    // Land one signal so satisfied_set is non-empty.
    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id, "approve", "alice", token))
        .await
        .unwrap();

    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let sset = ctx.suspension_satisfied_set();
    let mmap = ctx.suspension_member_map();
    assert!(key_exists(&tc, &sset).await, "satisfied_set must exist pre-cancel");
    // member_map may be empty if seed path didn't identify any
    // candidates for this tag shape; existence is therefore not
    // strictly required here — we only assert cancel cleanup wipes it.

    // Cancel the execution via direct FCALL.
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let cancel_keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new(WORKER_INST)),
        ctx.suspension_current(),
        ctx.waitpoint(&WaitpointId::from_uuid(uuid::Uuid::nil())),
        ctx.waitpoint_condition(&WaitpointId::from_uuid(uuid::Uuid::nil())),
        idx.suspension_timeout(),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_eligible(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.lane_blocked_dependencies(&lane_id),
        idx.lane_blocked_budget(&lane_id),
        idx.lane_blocked_quota(&lane_id),
        idx.lane_blocked_route(&lane_id),
        idx.lane_blocked_operator(&lane_id),
    ];
    let cancel_args: Vec<String> = vec![
        eid.to_string(),
        "test-cancel".to_owned(),
        "operator_override".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = cancel_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = cancel_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_cancel_execution", &kr, &ar)
        .await
        .expect("ff_cancel_execution");

    assert!(!key_exists(&tc, &sset).await, "satisfied_set must be deleted post-cancel");
    assert!(!key_exists(&tc, &mmap).await, "member_map must be deleted post-cancel");
}

#[tokio::test]
#[serial_test::serial]
async fn expire_deletes_satisfied_set_and_member_map() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    // Short timeout so expiry fires immediately.
    let wp_key = format!("wpk:{}", WaitpointId::new());
    let body = CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.clone()],
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: wp_key.clone(),
        },
        ResumeCondition::Composite(body),
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_timeout(
        TimestampMs::from_millis(TimestampMs::now().0 + 100),
        TimeoutBehavior::Fail,
    );
    let outcome = backend.suspend(&handle, args).await.unwrap();
    let (wp_id, token) = match outcome {
        SuspendOutcome::Suspended { details, .. } => {
            (details.waitpoint_id, details.waitpoint_token.token().clone())
        }
        other => panic!("got {other:?}"),
    };
    // Land one signal so satisfied_set is populated.
    let _ = backend
        .deliver_signal(deliver_args(&eid, wp_id.clone(), "approve", "alice", token))
        .await
        .unwrap();

    // Wait past timeout.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_condition(&wp_id),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        idx.suspension_timeout(),
        idx.lane_suspended(&lane_id),
        idx.lane_terminal(&lane_id),
        idx.lane_eligible(&lane_id),
        idx.lane_delayed(&lane_id),
        ctx.lease_history(),
    ];
    let args_fc: Vec<String> = vec![eid.to_string()];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args_fc.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_expire_suspension", &kr, &ar)
        .await
        .expect("ff_expire_suspension");

    let sset = ctx.suspension_satisfied_set();
    let mmap = ctx.suspension_member_map();
    assert!(!key_exists(&tc, &sset).await, "satisfied_set must be deleted post-expire");
    assert!(!key_exists(&tc, &mmap).await, "member_map must be deleted post-expire");
}

// ── RFC-014 Pattern 3 — heterogeneous subsystems, N distinct waitpoints ──

/// Canonical Pattern 3: suspend across three DISTINCT waitpoint_ids +
/// waitpoint_keys, wired via `ResumeCondition::all_of_waitpoints(...)`.
/// Delivery to the primary waitpoint plus the two extras (each with its
/// own minted HMAC token) must drive the `AllOf` tree to satisfaction.
#[tokio::test]
#[serial_test::serial]
async fn allof_three_waitpoints_resumes_when_all_fired() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    // Three heterogeneous waitpoint_keys.
    let wp_db_id = WaitpointId::new();
    let wp_db_key = format!("wpk:{wp_db_id}");
    let wp_cache_id = WaitpointId::new();
    let wp_cache_key = format!("wpk:{wp_cache_id}");
    let wp_flag_id = WaitpointId::new();
    let wp_flag_key = format!("wpk:{wp_flag_id}");

    let cond = ResumeCondition::all_of_waitpoints([
        wp_db_key.clone(),
        wp_cache_key.clone(),
        wp_flag_key.clone(),
    ]);

    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_db_id.clone(),
            waitpoint_key: wp_db_key.clone(),
        },
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_timeout(
        TimestampMs::from_millis(TimestampMs::now().0 + 60_000),
        TimeoutBehavior::Fail,
    )
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_cache_id.clone(),
        waitpoint_key: wp_cache_key.clone(),
    })
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_flag_id.clone(),
        waitpoint_key: wp_flag_key.clone(),
    });

    let outcome = backend
        .suspend(&handle, args)
        .await
        .expect("Pattern 3 suspend");
    let details = match outcome {
        SuspendOutcome::Suspended { details, .. } => details,
        other => panic!("expected Suspended, got {other:?}"),
    };

    // Primary is the DB waitpoint; extras are cache + flag in order.
    assert_eq!(details.waitpoint_id, wp_db_id);
    assert_eq!(details.waitpoint_key, wp_db_key);
    assert_eq!(details.additional_waitpoints.len(), 2);
    let cache_extra = &details.additional_waitpoints[0];
    let flag_extra = &details.additional_waitpoints[1];
    assert_eq!(cache_extra.waitpoint_id, wp_cache_id);
    assert_eq!(cache_extra.waitpoint_key, wp_cache_key);
    assert_eq!(flag_extra.waitpoint_id, wp_flag_id);
    assert_eq!(flag_extra.waitpoint_key, wp_flag_key);

    let tok_db = details.waitpoint_token.token().clone();
    let tok_cache = cache_extra.waitpoint_token.token().clone();
    let tok_flag = flag_extra.waitpoint_token.token().clone();

    // Signal #1: db-migration-complete → AllOf still pending.
    let r1 = backend
        .deliver_signal(deliver_args(
            &eid,
            wp_db_id.clone(),
            "db_migration_complete",
            "ops",
            tok_db,
        ))
        .await
        .unwrap();
    match r1 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint", "1st of 3 AllOf members");
        }
        other => panic!("got {other:?}"),
    }

    // Signal #2: cache-warmed via the cache waitpoint's OWN token.
    let r2 = backend
        .deliver_signal(deliver_args(
            &eid,
            wp_cache_id.clone(),
            "cache_warmed",
            "ops",
            tok_cache,
        ))
        .await
        .unwrap();
    match r2 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint", "2nd of 3 AllOf members");
        }
        other => panic!("got {other:?}"),
    }

    // Signal #3: feature-flag-set via the flag waitpoint's own token
    // → resume.
    let r3 = backend
        .deliver_signal(deliver_args(
            &eid,
            wp_flag_id,
            "feature_flag_set",
            "ops",
            tok_flag,
        ))
        .await
        .unwrap();
    match r3 {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("got {other:?}"),
    }
}

/// RFC-014 §10.2 cluster co-location — assert that all per-waitpoint
/// keys for a multi-waitpoint suspend share the partition's hash tag
/// (`{fp:N}`) so a single Valkey Function call stays on one slot under
/// RFC-011 co-location. Runs in standalone mode too — the invariant is
/// a pure-string property of the key layout.
#[tokio::test]
#[serial_test::serial]
async fn multi_waitpoint_keys_colocated_under_partition_hash_tag() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_a_id = WaitpointId::new();
    let wp_a_key = format!("wpk:{wp_a_id}");
    let wp_b_id = WaitpointId::new();
    let wp_b_key = format!("wpk:{wp_b_id}");
    let wp_c_id = WaitpointId::new();
    let wp_c_key = format!("wpk:{wp_c_id}");

    let cond = ResumeCondition::all_of_waitpoints([
        wp_a_key.clone(),
        wp_b_key.clone(),
        wp_c_key.clone(),
    ]);

    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_a_id.clone(),
            waitpoint_key: wp_a_key.clone(),
        },
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_timeout(
        TimestampMs::from_millis(TimestampMs::now().0 + 60_000),
        TimeoutBehavior::Fail,
    )
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_b_id.clone(),
        waitpoint_key: wp_b_key,
    })
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_c_id.clone(),
        waitpoint_key: wp_c_key,
    });

    let outcome = backend.suspend(&handle, args).await.unwrap();
    match outcome {
        SuspendOutcome::Suspended { .. } => {}
        other => panic!("expected Suspended, got {other:?}"),
    }

    // RFC-011 §11 — execution-scoped keys are tagged `{fp:N}` where N
    // is the execution's partition index.
    let partition = execution_partition(&eid, &config());
    let tag = partition.hash_tag(); // e.g. "{fp:12}"
    let ctx = ExecKeyContext::new(&partition, &eid);

    let suspension_key = ctx.suspension_current();
    let wp_a_hash = ctx.waitpoint(&wp_a_id);
    let wp_b_hash = ctx.waitpoint(&wp_b_id);
    let wp_c_hash = ctx.waitpoint(&wp_c_id);
    let wp_a_cond = ctx.waitpoint_condition(&wp_a_id);
    let wp_b_cond = ctx.waitpoint_condition(&wp_b_id);
    let wp_c_cond = ctx.waitpoint_condition(&wp_c_id);

    for key in [
        &suspension_key,
        &wp_a_hash,
        &wp_b_hash,
        &wp_c_hash,
        &wp_a_cond,
        &wp_b_cond,
        &wp_c_cond,
    ] {
        assert!(
            key.contains(&tag),
            "RFC-011 co-location violated: key {key:?} missing partition hash tag {tag}"
        );
    }
}

/// RFC-014 Pattern 3 cleanup: cancelling a multi-waitpoint suspension
/// must close every extra waitpoint hash (not just the primary), so
/// no dangling active waitpoints remain on the execution.
#[tokio::test]
#[serial_test::serial]
async fn cancel_closes_every_waitpoint_in_multi_binding() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    let handle = create_and_claim_handle(&tc, &eid).await;
    let backend = build_backend(&tc).await;

    let wp_a_id = WaitpointId::new();
    let wp_a_key = format!("wpk:{wp_a_id}");
    let wp_b_id = WaitpointId::new();
    let wp_b_key = format!("wpk:{wp_b_id}");

    let cond = ResumeCondition::all_of_waitpoints([
        wp_a_key.clone(),
        wp_b_key.clone(),
    ]);
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_a_id.clone(),
            waitpoint_key: wp_a_key,
        },
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_timeout(
        TimestampMs::from_millis(TimestampMs::now().0 + 60_000),
        TimeoutBehavior::Fail,
    )
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_b_id.clone(),
        waitpoint_key: wp_b_key,
    });
    let _ = backend.suspend(&handle, args).await.unwrap();

    // Cancel — operator override (no lease fields required).
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let cancel_keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new(WORKER_INST)),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_a_id),
        ctx.waitpoint_condition(&wp_a_id),
        idx.suspension_timeout(),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_eligible(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.lane_blocked_dependencies(&lane_id),
        idx.lane_blocked_budget(&lane_id),
        idx.lane_blocked_quota(&lane_id),
        idx.lane_blocked_route(&lane_id),
        idx.lane_blocked_operator(&lane_id),
    ];
    let cancel_args: Vec<String> = vec![
        eid.to_string(),
        "test-cancel".to_owned(),
        "operator_override".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = cancel_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = cancel_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_cancel_execution", &kr, &ar)
        .await
        .expect("ff_cancel_execution");

    // Primary waitpoint hash must be closed (cancel path touches it
    // via K.waitpoint_hash). Extra waitpoint hash must ALSO be closed
    // — that is the Pattern 3 cleanup invariant.
    let wp_b_hash_key = ctx.waitpoint(&wp_b_id);
    let state: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(wp_b_hash_key.as_str())
        .arg("waitpoint_id")
        .execute()
        .await
        .expect("HGET");
    assert_eq!(
        state.as_deref(),
        Some(wp_b_id.to_string().as_str()),
        "extra waitpoint hash must still exist post-cancel for audit"
    );
    let close_reason: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(wp_b_hash_key.as_str())
        .arg("close_reason")
        .execute()
        .await
        .expect("HGET close_reason");
    assert_eq!(
        close_reason.as_deref(),
        Some("cancelled"),
        "Pattern 3 extra waitpoint must be closed with reason=cancelled on cancel"
    );
    let state_field: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(wp_b_hash_key.as_str())
        .arg("state")
        .execute()
        .await
        .expect("HGET state");
    assert_eq!(
        state_field.as_deref(),
        Some("closed"),
        "Pattern 3 extra waitpoint state must be 'closed' on cancel"
    );
}
