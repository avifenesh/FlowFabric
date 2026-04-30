//! PR-D1 / Test 1 — Lifecycle happy path.
//!
//! Proves the readiness harness can drive a full execution lifecycle
//! end-to-end through the SDK + ff-server + Valkey path:
//!
//!   create_flow → create_execution → add_to_flow → worker.claim_next
//!   → update_progress → append_frame → complete → poll-to-terminal.
//!
//! This is a "the whole plumbing works" test, not a regression. It
//! runs only under the `readiness` feature + `#[ignore]` so the
//! default `cargo test --workspace` pass is unaffected.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test lifecycle_happy_path -- --ignored --test-threads=1
//!
//! RED-proof: commenting out `task.complete(...)` makes step 11
//! (poll-to-terminal) time out — proving the assertion exercises the
//! lifecycle transition, not just type-checks. Captured transcript:
//! `tests/red-proofs/lifecycle_happy_path.log`.
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster, TEST_PARTITION_CONFIG};
use serde_json::json;

const TEST_NAME: &str = "lifecycle_happy_path";
const LANE: &str = "readiness-happy";
const NS: &str = "readiness-happy-ns";

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn lifecycle_happy_path() {
    let t_start = Instant::now();

    // 1. Valkey harness — flushes + loads the FF library.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;

    // 2. In-process ff-server — waits for /healthz 200.
    let server = InProcessServer::start(LANE).await;

    // 3. Flow + 4. execution + 5. membership.
    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "readiness-happy".to_owned(),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow");

    let eid = ExecutionId::for_flow(&flow_id, &TEST_PARTITION_CONFIG);
    let deadline_at = TimestampMs::from_millis(now.0 + 60_000);
    server
        .server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "readiness-happy".to_owned(),
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
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now,
        })
        .await
        .expect("add_execution_to_flow");

    // 6. Spawn in-process SDK worker using direct-valkey-claim.
    let worker_config = ff_sdk::WorkerConfig {
        backend: Some(ff_readiness_tests::valkey::backend_config_from_env()),
        worker_id: WorkerId::new("readiness-happy-worker"),
        worker_instance_id: WorkerInstanceId::new("readiness-happy-inst"),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 4,
    partition_config: None,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config)
        .await
        .expect("FlowFabricWorker::connect");

    // 7. Claim the single eligible execution. Bounded so an
    //    eligibility regression fails fast instead of hanging the
    //    ignored run.
    let t_claim = Instant::now();
    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next timed out after 10s — execution never became eligible");
    let claim_ms = t_claim.elapsed().as_millis() as u64;
    assert_eq!(
        task.execution_id(),
        &eid,
        "claimed execution should match the one we created"
    );

    // 8. Progress update.
    task.update_progress(50, "halfway")
        .await
        .expect("update_progress");

    // 9. Append a progress frame.
    let frame_outcome = task
        .append_frame(
            "progress",
            &serde_json::to_vec(&json!({"step": "mid"})).expect("encode frame"),
            None,
        )
        .await
        .expect("append_frame");
    assert!(
        !frame_outcome.stream_id.is_empty(),
        "append_frame must mint a non-empty stream_id"
    );
    assert_eq!(frame_outcome.frame_count, 1);

    // Capture attempt_index before `complete` consumes the task so we
    // can inspect the attempt's stream after terminal.
    let attempt_index = task.attempt_index();

    // 10. Complete. (Removing this line is the RED-proof.)
    task.complete(Some(b"readiness-ok".to_vec()))
        .await
        .expect("task.complete");

    // 11. Poll the SDK `describe_execution` snapshot until terminal.
    //     Uses the same read surface consumers see, not the Server's
    //     internal `get_execution`, so the test exercises the full
    //     SDK + Valkey path end-to-end.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (public_state, total_attempts) = loop {
        let snapshot = worker
            .describe_execution(&eid)
            .await
            .expect("describe_execution")
            .expect("snapshot must exist for a just-created execution");
        if matches!(
            snapshot.public_state,
            PublicState::Completed
                | PublicState::Failed
                | PublicState::Cancelled
                | PublicState::Expired
                | PublicState::Skipped
        ) {
            break (snapshot.public_state, snapshot.total_attempt_count);
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for execution {eid} to reach terminal \
                 (5s); last public_state={:?}",
                snapshot.public_state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // 12. Assert terminal shape. `public_state == Completed` is the
    //     SDK-surface analogue of `terminal_outcome == Success`.
    assert_eq!(
        public_state,
        PublicState::Completed,
        "derived public state should be Completed"
    );
    assert_eq!(
        total_attempts, 1,
        "a single happy-path attempt should have been recorded"
    );

    // Stream must carry at least one progress frame (the one we
    // appended, plus whatever the engine emits on complete).
    let stream_partition = execution_partition(&eid, &TEST_PARTITION_CONFIG);
    let stream_ctx = ExecKeyContext::new(&stream_partition, &eid);
    let stream_key = stream_ctx.stream(attempt_index);
    let stream_len: i64 = tc
        .client()
        .cmd("XLEN")
        .arg(stream_key.as_str())
        .execute()
        .await
        .expect("XLEN stream");
    assert!(
        stream_len >= 1,
        "stream should contain at least the progress frame, got {stream_len}"
    );

    // Explicit teardown — drains engine + background tasks so state
    // does not leak into the next serial readiness test.
    drop(worker);
    server.shutdown().await;

    // 13. Evidence JSON.
    evidence::write(
        TEST_NAME,
        &json!({
            "test": TEST_NAME,
            "total_ms": t_start.elapsed().as_millis() as u64,
            "claim_ms": claim_ms,
            "valkey_version": valkey_version,
            "execution_id": eid.to_string(),
            "flow_id": flow_id.to_string(),
            "assertions": {
                "public_state": format!("{:?}", public_state),
                "total_attempt_count": total_attempts,
                "stream_len": stream_len,
            },
        }),
    );
}
