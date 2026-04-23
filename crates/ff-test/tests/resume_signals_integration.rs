//! Integration tests for `ClaimedTask::resume_signals()` (closes #36 item 3).
//!
//! PR #35 landed the API with 3 unit tests for one helper function. The
//! per-matcher HMGET loop + pipelined HGETALL/GET fetch are exercised
//! end-to-end here: suspend → deliver_signal → reclaim → resume_signals
//! and assert the matched-signal list + payload survive the round-trip.
//!
//! Covers:
//!   - single matcher + payload → 1 ResumeSignal with payload bytes
//!   - "all" mode with 2 matchers → 2 ResumeSignals (both required to resume)
//!   - payload-less signal → payload field is None
//!   - resume_signals() returns Vec::new() when the ClaimedTask is NOT
//!     a resumed attempt (e.g. the worker's first claim of a never-
//!     suspended execution).
//!
//! Run with: cargo test -p ff-test --test resume_signals_integration -- --test-threads=1

use ff_sdk::task::{
    CompositeBody, ResumeCondition, ResumePolicy, Signal, SignalMatcher, SignalOutcome,
    SuspensionReasonCode, TimeoutBehavior,
};
use ff_test::fixtures::TestCluster;

const LANE: &str = "resume-sig-lane";
const NS: &str = "resume-sig-ns";

async fn build_worker(name_suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: ff_core::types::WorkerId::new(format!("resume-sig-worker-{name_suffix}")),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "resume-sig-inst-{name_suffix}"
        )),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 10,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

/// Write the partition config the SDK reads on claim. Mirrors the
/// setup used by test_sdk_suspend_signal_resume_reclaim.
async fn seed_partition_config(tc: &TestCluster) {
    let cfg = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(cfg.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(cfg.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(cfg.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn create_and_claim(
    tc: &TestCluster,
    worker: &ff_sdk::FlowFabricWorker,
    payload_hint: &str,
) -> (ff_core::types::ExecutionId, ff_sdk::task::ClaimedTask) {
    let eid = tc.new_execution_id();

    // Minimal direct-FCALL ff_create_execution call so the test
    // file doesn't need to import e2e_lifecycle's fcall_create_execution
    // (it's file-private).
    let config = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let partition = ff_core::partition::execution_partition(&eid, &config);
    let ctx = ff_core::keys::ExecKeyContext::new(&partition, &eid);
    let idx = ff_core::keys::IndexKeys::new(&partition);
    let lane_id = ff_core::types::LaneId::new(LANE);

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
        payload_hint.to_owned(),
        "0".to_owned(),
        "resume-sig-test".to_owned(),
        "{}".to_owned(),
        r#"{"test":"resume_signals"}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: ferriskey::Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");

    let task = worker
        .claim_next()
        .await
        .unwrap()
        .expect("should claim freshly-created execution");
    (eid, task)
}

#[tokio::test]
#[serial_test::serial]
async fn resume_signals_returns_single_matched_signal_with_payload() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("single").await;

    let (eid, task) = create_and_claim(&tc, &worker, "single_sig").await;

    // Suspend with one matcher.
    let wp_key = format!("wpk:{}", uuid::Uuid::new_v4());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForApproval,
            ResumeCondition::Single {
                waitpoint_key: wp_key.clone(),
                matcher: SignalMatcher::ByName("approve".into()),
            },
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 60_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await
        .unwrap();
    let waitpoint_id = outcome.details.waitpoint_id.clone();
    let token = outcome.details.waitpoint_token.token().clone();

    // Deliver signal with a distinctive payload.
    let payload_bytes = b"approved by jane".to_vec();
    let sig = Signal {
        signal_name: "approve".into(),
        signal_category: "decision".into(),
        payload: Some(payload_bytes.clone()),
        source_type: "test".into(),
        source_identity: "jane".into(),
        idempotency_key: None,
        waitpoint_token: token,
    };
    match worker.deliver_signal(&eid, &waitpoint_id, sig).await.unwrap() {
        SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume, got {other:?}"),
    }

    // Re-claim and inspect resume_signals.
    let task2 = worker
        .claim_next()
        .await
        .unwrap()
        .expect("should re-claim resumed execution");
    let signals = task2.resume_signals().await.unwrap();
    assert_eq!(signals.len(), 1, "single matcher → exactly 1 signal");
    let s = &signals[0];
    assert_eq!(s.signal_name, "approve");
    assert_eq!(s.signal_category, "decision");
    assert_eq!(s.source_identity, "jane");
    assert_eq!(
        s.payload.as_deref(),
        Some(payload_bytes.as_slice()),
        "payload bytes must round-trip"
    );
    assert!(
        s.accepted_at.0 > 0,
        "accepted_at must be a real server timestamp"
    );

    task2.complete(None).await.unwrap();
}

// RFC-014 lands the composite `AllOf` variant. This test exercises a
// single-waitpoint AllOf with two distinct-name matchers; both signals
// must arrive before the suspension resumes.
#[tokio::test]
#[serial_test::serial]
async fn resume_signals_all_mode_returns_both_matchers() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("all").await;

    let (eid, task) = create_and_claim(&tc, &worker, "all_mode").await;

    let wp_key = format!("wpk:{}", uuid::Uuid::new_v4());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForApproval,
            ResumeCondition::Composite(CompositeBody::AllOf {
                members: vec![
                    ResumeCondition::Single {
                        waitpoint_key: wp_key.clone(),
                        matcher: SignalMatcher::ByName("reviewer_a".into()),
                    },
                    ResumeCondition::Single {
                        waitpoint_key: wp_key.clone(),
                        matcher: SignalMatcher::ByName("reviewer_b".into()),
                    },
                ],
            }),
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 60_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await
        .unwrap();
    let waitpoint_id = outcome.details.waitpoint_id.clone();
    let token = outcome.details.waitpoint_token.token().clone();

    // Deliver reviewer_a first — suspension must stay open ("all" mode
    // requires both matchers satisfied).
    let sig_a = Signal {
        signal_name: "reviewer_a".into(),
        signal_category: "decision".into(),
        payload: Some(b"a-approved".to_vec()),
        source_type: "test".into(),
        source_identity: "a".into(),
        idempotency_key: None,
        waitpoint_token: token.clone(),
    };
    match worker.deliver_signal(&eid, &waitpoint_id, sig_a).await.unwrap() {
        SignalOutcome::Accepted { .. } => {}
        other => panic!("expected Accepted after first of two matchers, got {other:?}"),
    }

    // Deliver reviewer_b → resume.
    let sig_b = Signal {
        signal_name: "reviewer_b".into(),
        signal_category: "decision".into(),
        payload: Some(b"b-approved".to_vec()),
        source_type: "test".into(),
        source_identity: "b".into(),
        idempotency_key: None,
        waitpoint_token: token,
    };
    match worker.deliver_signal(&eid, &waitpoint_id, sig_b).await.unwrap() {
        SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume, got {other:?}"),
    }

    let task2 = worker
        .claim_next()
        .await
        .unwrap()
        .expect("should re-claim");
    let mut signals = task2.resume_signals().await.unwrap();
    signals.sort_by(|l, r| l.signal_name.cmp(&r.signal_name));
    assert_eq!(signals.len(), 2, "all mode → both matched signals returned");
    assert_eq!(signals[0].signal_name, "reviewer_a");
    assert_eq!(signals[0].payload.as_deref(), Some(b"a-approved".as_slice()));
    assert_eq!(signals[1].signal_name, "reviewer_b");
    assert_eq!(signals[1].payload.as_deref(), Some(b"b-approved".as_slice()));

    task2.complete(None).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn resume_signals_payload_none_when_signal_payload_omitted() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("nopayload").await;

    let (eid, task) = create_and_claim(&tc, &worker, "no_payload").await;

    let wp_key = format!("wpk:{}", uuid::Uuid::new_v4());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForSignal,
            ResumeCondition::Single {
                waitpoint_key: wp_key,
                matcher: SignalMatcher::ByName("ping".into()),
            },
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 60_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await
        .unwrap();
    let waitpoint_id = outcome.details.waitpoint_id.clone();
    let token = outcome.details.waitpoint_token.token().clone();

    let sig = Signal {
        signal_name: "ping".into(),
        signal_category: "event".into(),
        payload: None,
        source_type: "test".into(),
        source_identity: "pinger".into(),
        idempotency_key: None,
        waitpoint_token: token,
    };
    worker.deliver_signal(&eid, &waitpoint_id, sig).await.unwrap();

    let task2 = worker.claim_next().await.unwrap().unwrap();
    let signals = task2.resume_signals().await.unwrap();
    assert_eq!(signals.len(), 1);
    assert!(
        signals[0].payload.is_none(),
        "signal delivered without payload → ResumeSignal.payload is None, got {:?}",
        signals[0].payload
    );

    task2.complete(None).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn resume_signals_empty_for_non_resumed_claim() {
    // First-ever claim of a never-suspended execution must return an
    // empty Vec, not an error. This is the discriminator workers use
    // to branch between "fresh execution" and "resumed from signal."
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("fresh").await;

    let (_eid, task) = create_and_claim(&tc, &worker, "fresh").await;

    let signals = task.resume_signals().await.unwrap();
    assert!(
        signals.is_empty(),
        "fresh claim (no prior suspension) → empty Vec, got {signals:?}"
    );

    task.complete(None).await.unwrap();
}
