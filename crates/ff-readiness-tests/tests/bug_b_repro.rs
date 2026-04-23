//! Bug B repro — scheduler-routed claim (`claim_via_server` /
//! `claim_from_grant`) does NOT resume suspended executions.
//!
//! Scenario (mirrors `examples/coding-agent`):
//!   1. Worker claims execution via HTTP `/v1/workers/{id}/claim`
//!      (`claim_via_server`).
//!   2. Worker `suspend`s with a `review` matcher. exec_core flips to
//!      `lifecycle_phase=suspended`, `attempt_state=attempt_interrupted`.
//!   3. Signal delivered; resume condition satisfied. exec_core flips to
//!      `lifecycle_phase=runnable`, `eligibility_state=eligible_now`,
//!      `blocking_reason=waiting_for_worker`, `attempt_state` stays
//!      `attempt_interrupted`. Execution is ZADD'd back into
//!      `lane:default:eligible`.
//!   4. Worker polls `/v1/workers/{id}/claim` and tries to claim the
//!      grant. Expected: task hands back, worker completes. Actual:
//!      `ff_claim_execution` rejects with `use_claim_resumed_execution`
//!      (execution.lua:382) because the SDK's HTTP-path
//!      `claim_from_grant` does NOT fall back to
//!      `claim_resumed_execution` the way direct-claim `claim_next`
//!      does (worker.rs:819-852).
//!
//! RED-proof: this test should fail on current main. The
//! `direct-valkey-claim` equivalent (`waitpoint_hmac_roundtrip`) passes
//! because `claim_next` has the fallback branch.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test bug_b_repro -- --ignored --test-threads=1 --nocapture
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{TestCluster, TEST_PARTITION_CONFIG};
use ff_sdk::task::{
    ClaimedTask, ResumeCondition, ResumePolicy, Signal, SignalMatcher, SignalOutcome,
    SuspensionReasonCode, TimeoutBehavior,
};

const LANE: &str = "bug-b-lane";
const NS: &str = "bug-b-ns";
const SIGNAL_NAME: &str = "review";

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn bug_b_resume_via_server_claim() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = InProcessServer::start(LANE).await;

    // Worker config. Server-routed claim — no direct-valkey-claim feature reliance.
    let worker_config = ff_sdk::WorkerConfig {
        backend: ff_readiness_tests::valkey::backend_config_from_env(),
        worker_id: WorkerId::new("bug-b-worker"),
        worker_instance_id: WorkerInstanceId::new("bug-b-inst"),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 4,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config)
        .await
        .expect("FlowFabricWorker::connect");

    // Build an admin client for the HTTP claim path.
    let admin = ff_sdk::FlowFabricAdminClient::new(server.base_url.clone())
        .expect("FlowFabricAdminClient::new");

    let lane = LaneId::new(LANE);

    // ── Step 1: create + claim-via-server ──
    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "bug-b".to_owned(),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow");

    let eid = ExecutionId::for_flow(&flow_id, &TEST_PARTITION_CONFIG);
    let deadline_at = TimestampMs::from_millis(now.0 + 120_000);
    server
        .server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: lane.clone(),
            execution_kind: "bug-b".to_owned(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "readiness".to_owned(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: Some(deadline_at),
            partition_id: execution_partition(&eid, &TEST_PARTITION_CONFIG).index,
            now,
        })
        .await
        .expect("create_execution");
    server
        .server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id,
            execution_id: eid.clone(),
            now,
        })
        .await
        .expect("add_execution_to_flow");

    // Claim via server (HTTP path — the code path Bug B is about).
    let task = claim_via_server(&worker, &admin, &lane).await;
    assert_eq!(task.execution_id(), &eid, "claim should hand back our exec");

    // ── Step 2: suspend ──
    let wp_key = format!("wpk:{}", ff_core::types::WaitpointId::new());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForOperatorReview,
            ResumeCondition::Single {
                waitpoint_key: wp_key.clone(),
                matcher: SignalMatcher::ByName(SIGNAL_NAME.into()),
            },
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 60_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => panic!("suspend: {e}"),
    };
    // RFC-013 Stage 1d — strict `suspend` returns `SuspendedHandle`.
    let wp_id = outcome.details.waitpoint_id.clone();
    let token = outcome.details.waitpoint_token.token().clone();

    // ── Step 3: deliver signal — resume condition satisfied ──
    let signal = Signal {
        signal_name: SIGNAL_NAME.into(),
        signal_category: "decision".into(),
        payload: Some(b"{\"approved\":true}".to_vec()),
        source_type: "readiness".into(),
        source_identity: "bug-b".into(),
        idempotency_key: None,
        waitpoint_token: token,
    };
    let sig_outcome = worker
        .deliver_signal(&eid, &wp_id, signal)
        .await
        .expect("deliver_signal");
    assert!(
        matches!(sig_outcome, SignalOutcome::TriggeredResume { .. }),
        "expected TriggeredResume, got {sig_outcome:?}"
    );

    // ── State dump at the broken moment ──
    // Capture exec_core + eligible zset membership so the failure mode
    // is self-documenting.
    let partition = execution_partition(&eid, &TEST_PARTITION_CONFIG);
    let core_key = ff_core::keys::ExecKeyContext::new(&partition, &eid).core();
    let eligible_key = ff_core::keys::IndexKeys::new(&partition)
        .lane_eligible(&lane);
    // HGETALL under ferriskey returns a map-typed value; use raw Value
    // and stringify so the dump works regardless of RESP reply shape.
    let core_val: ferriskey::Value = tc
        .client()
        .cmd("HGETALL")
        .arg(core_key.as_str())
        .execute()
        .await
        .expect("HGETALL exec_core");
    let in_eligible: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(eligible_key.as_str())
        .arg(eid.to_string())
        .execute()
        .await
        .ok()
        .flatten();
    eprintln!("── Valkey state at broken moment ──");
    eprintln!("exec_core ({core_key}) = {core_val:#?}");
    eprintln!(
        "ZSCORE {eligible_key} {eid} -> {:?} (Some(_) = member)",
        in_eligible
    );

    // ── Step 4: re-claim via server. Bounded so the hang surfaces. ──
    let reclaim_deadline = Instant::now() + Duration::from_secs(6);
    let task2 = loop {
        if Instant::now() >= reclaim_deadline {
            panic!(
                "BUG B REPRODUCED: `claim_via_server` never re-claimed the resumed \
                 execution {eid}. exec_core shows lifecycle_phase=runnable + \
                 eligibility_state=eligible_now + attempt_state=attempt_interrupted, \
                 and the execution IS in lane:{LANE}:eligible, but the scheduler \
                 issues a grant and `claim_from_grant` -> `ff_claim_execution` \
                 rejects with `use_claim_resumed_execution` (execution.lua:382). \
                 The SDK HTTP-path claim helper does not fall back to \
                 `ff_claim_resumed_execution` the way direct-claim `claim_next` \
                 does (worker.rs:819-852)."
            );
        }
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(t)) if t.execution_id() == &eid => break t,
            Ok(Some(_other)) => { /* drop, lease will expire */ }
            Ok(None) => {}
            Err(e) => {
                eprintln!("claim_via_server error (repro-relevant): {e}");
                // Surface immediately — an Err instead of Ok(None) is
                // itself the bug shape.
                panic!("BUG B: claim_via_server returned Err: {e}");
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // If we got here, the fix worked.
    task2
        .complete(Some(b"bug-b-fixed".to_vec()))
        .await
        .expect("complete after re-claim");

    // Await terminal.
    let term_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = worker
            .describe_execution(&eid)
            .await
            .expect("describe_execution")
            .expect("snapshot");
        if matches!(
            snap.public_state,
            PublicState::Completed
                | PublicState::Failed
                | PublicState::Cancelled
                | PublicState::Expired
                | PublicState::Skipped
        ) {
            assert_eq!(snap.public_state, PublicState::Completed);
            break;
        }
        if Instant::now() >= term_deadline {
            panic!("timed out awaiting terminal");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    drop(worker);
    server.shutdown().await;
}

async fn claim_via_server(
    worker: &ff_sdk::FlowFabricWorker,
    admin: &ff_sdk::FlowFabricAdminClient,
    lane: &LaneId,
) -> ClaimedTask {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match worker.claim_via_server(admin, lane, 10_000).await {
                Ok(Some(t)) => break t,
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("claim_via_server err: {e}"),
            }
        }
    })
    .await
    .expect("initial claim_via_server timed out")
}
