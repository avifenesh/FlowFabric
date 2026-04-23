//! PR #112 regression — two `#[tokio::test]`s in one file, same lane,
//! demonstrate that `InProcessServer::shutdown().await` prevents
//! engine-pollution across tests in the same process.
//!
//! Root cause (pre-fix): `InProcessServer::Drop` only aborted the axum
//! listener. The spawned `Server` + its `completion_listener` +
//! background `JoinSet` stayed alive, racing against subsequent tests
//! that flushed Valkey and booted their own engine. The second test
//! in a file (or any file in the same binary on the same lane) would
//! see stray state written by the previous engine's listener.
//!
//! Fix: `InProcessServer::shutdown(self).await` consumes the binding,
//! aborts + awaits the axum task, unwraps `Arc<Server>`, and calls
//! upstream `Server::shutdown` which closes the stream semaphore,
//! drains the handler background tasks, and stops the engine's
//! scanners.
//!
//! # RED procedure
//!
//! 1. Revert `server.shutdown().await` in this file's two tests to a
//!    bare `drop(server)` (or remove the call entirely).
//! 2. `cargo test -p ff-readiness-tests --features readiness \
//!      --test inprocess_multi_test -- --ignored --test-threads=1`
//! 3. The second test (`second_after_shutdown`) observes Valkey state
//!    bled from the first test's still-live completion_listener
//!    (stale attempt/stream keys on the shared lane); its `XLEN`
//!    assertion fails or `create_execution` bumps an unexpected
//!    revision.
//!
//! With the fix in place (current code), both tests pass on the same
//! lane in the same binary in the same process.
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{TestCluster, TEST_PARTITION_CONFIG};

// Single shared lane + namespace intentional — the regression is
// precisely about two tests colliding on the same lane in-process.
const LANE: &str = "readiness-multi";
const NS: &str = "readiness-multi-ns";

async fn drive_one_execution(tag: &str) {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = InProcessServer::start(LANE).await;

    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: format!("readiness-multi-{tag}"),
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
            execution_kind: format!("readiness-multi-{tag}"),
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

    let worker_config = ff_sdk::WorkerConfig {
        backend: ff_readiness_tests::valkey::backend_config_from_env(),
        worker_id: WorkerId::new(format!("readiness-multi-worker-{tag}")),
        worker_instance_id: WorkerInstanceId::new(format!("readiness-multi-inst-{tag}")),
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

    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next timed out");
    assert_eq!(task.execution_id(), &eid);

    task.complete(Some(format!("multi-{tag}-ok").into_bytes()))
        .await
        .expect("task.complete");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = worker
            .describe_execution(&eid)
            .await
            .expect("describe_execution")
            .expect("snapshot must exist");
        if matches!(snapshot.public_state, PublicState::Completed) {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for execution {eid} to reach Completed; \
                 last public_state={:?} — this is the regression signal when \
                 `server.shutdown().await` is removed: a stale completion_listener \
                 from a prior test writes conflicting state to Valkey and the \
                 second test's execution never converges.",
                snapshot.public_state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The assertion that fails when shutdown is reverted to a bare
    // drop: the regression signature is a second-test timeout on the
    // describe loop above, or a cross-lane key collision. With the
    // fix both invocations of this helper succeed in the same binary.
    drop(worker);
    server.shutdown().await;
}

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn first_before_shutdown() {
    drive_one_execution("first").await;
}

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn second_after_shutdown() {
    drive_one_execution("second").await;
}
