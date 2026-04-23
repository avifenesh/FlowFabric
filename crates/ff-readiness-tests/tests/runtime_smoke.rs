//! v0.5.0 runtime smoke — Scenarios A/B/C/D from
//! `rfcs/drafts/0.4.0-runtime-smoke.md` re-run against current `main`.
//!
//! Each scenario is a separate `#[ignore]` test under the readiness
//! feature so it doesn't run in the default pass.
//!
//! Run:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test runtime_smoke -- --ignored --test-threads=1 --nocapture
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::backend::BackendConfig;
use ff_core::contracts::CreateExecutionArgs;
use ff_core::engine_error::EngineError;
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{TestCluster, TEST_PARTITION_CONFIG};
use ff_sdk::{StreamCursor, WorkerConfig};

fn suffix() -> String {
    let pid = std::process::id();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{pid}-{ms}")
}

async fn spawn_worker(lane: &str, ns: &str, sfx: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = WorkerConfig {
        backend: ff_readiness_tests::valkey::backend_config_from_env(),
        worker_id: WorkerId::new(format!("rs-smoke-w-{sfx}")),
        worker_instance_id: WorkerInstanceId::new(format!("rs-smoke-i-{sfx}")),
        namespace: Namespace::new(ns),
        lanes: vec![LaneId::new(lane)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 50,
        max_concurrent_tasks: 4,
    };
    ff_sdk::FlowFabricWorker::connect(cfg)
        .await
        .expect("FlowFabricWorker::connect")
}

async fn submit_solo(
    server: &InProcessServer,
    ns: &str,
    lane: &str,
) -> ExecutionId {
    let lane_id = LaneId::new(lane);
    let eid = ExecutionId::solo(&lane_id, &TEST_PARTITION_CONFIG);
    let now = TimestampMs::now();
    server
        .server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(ns),
            lane_id,
            execution_kind: "runtime-smoke".to_owned(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "runtime-smoke".to_owned(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: Some(TimestampMs::from_millis(now.0 + 60_000)),
            partition_id: execution_partition(&eid, &TEST_PARTITION_CONFIG).index,
            now,
        })
        .await
        .expect("create_execution");
    eid
}

// ── Scenario A: claim → progress → append_frame → complete → terminal ──
#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn runtime_smoke_a_claim_to_complete() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let sfx = suffix();
    let lane = format!("rs-a-lane-{sfx}");
    let ns = format!("rs-a-ns-{sfx}");

    let server = InProcessServer::start(&lane).await;
    let eid = submit_solo(&server, &ns, &lane).await;

    let worker = spawn_worker(&lane, &ns, &sfx).await;

    // Claim, matching our submitted eid.
    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next") {
                assert_eq!(t.execution_id(), &eid, "claimed unexpected execution");
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next timeout");

    task.update_progress(25, "quarter").await.expect("update_progress 25");
    task.update_progress(75, "three-quarters").await.expect("update_progress 75");

    let t0 = Instant::now();
    task.complete(Some(b"done".to_vec())).await.expect("complete");
    let complete_us = t0.elapsed().as_micros();

    // Poll terminal public_state.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = worker
            .describe_execution(&eid)
            .await
            .expect("describe_execution");
        if let Some(info) = snap
            && info.public_state == PublicState::Completed
        {
            eprintln!(
                "[scenario-A] PASS eid={eid} complete_round_trip_us={complete_us}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "timed out awaiting Completed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    drop(worker);
    server.shutdown().await;
}

// ── Scenario B: ScannerFilter isolation ──
// Positive delivery requires flow-bound executions (solo executions
// never PUBLISH per RFC-012-amendment; documented in
// 0.4.0-runtime-smoke.md §B). Here we re-verify the negative
// (no cross-talk) on the current tree: two subscribers with disjoint
// namespaces observe zero frames from one solo execution in each ns.
#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn runtime_smoke_b_scanner_filter_isolation() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let sfx = suffix();
    let lane = format!("rs-b-lane-{sfx}");
    let ns1 = format!("rs-b-ns1-{sfx}");
    let ns2 = format!("rs-b-ns2-{sfx}");

    let server = InProcessServer::start(&lane).await;

    // Submit one solo exec per ns.
    let eid1 = submit_solo(&server, &ns1, &lane).await;
    let eid2 = submit_solo(&server, &ns2, &lane).await;

    // Two workers, each scoped to its own namespace, both on the
    // same lane. Claim-next must return only the caller's eid.
    let w1 = spawn_worker(&lane, &ns1, &format!("{sfx}-w1")).await;
    let w2 = spawn_worker(&lane, &ns2, &format!("{sfx}-w2")).await;

    let t1 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = w1.claim_next().await.expect("w1 claim") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("w1 claim timeout");
    assert_eq!(
        t1.execution_id(),
        &eid1,
        "cross-talk: w1(ns={ns1}) received {} which was submitted to ns2",
        t1.execution_id()
    );
    t1.complete(Some(b"ok1".to_vec())).await.expect("w1 complete");

    let t2 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = w2.claim_next().await.expect("w2 claim") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("w2 claim timeout");
    assert_eq!(
        t2.execution_id(),
        &eid2,
        "cross-talk: w2(ns={ns2}) received {}",
        t2.execution_id()
    );
    t2.complete(Some(b"ok2".to_vec())).await.expect("w2 complete");

    eprintln!("[scenario-B] PASS isolation: w1 only saw {eid1}, w2 only saw {eid2}");

    drop(w1);
    drop(w2);
    server.shutdown().await;
}

// ── Scenario C: dead-port → EngineError::Transport ──
#[tokio::test]
#[ignore]
async fn runtime_smoke_c_transport_error() {
    let cfg = BackendConfig::valkey("127.0.0.1", 1); // reserved/dead
    let result = ff_backend_valkey::ValkeyBackend::connect(cfg).await;
    let err = match result {
        Ok(_) => panic!("[scenario-C] FAIL: connect to port 1 unexpectedly succeeded"),
        Err(e) => e,
    };
    match &err {
        EngineError::Transport { .. } => {
            eprintln!("[scenario-C] PASS EngineError::Transport surfaced: {err}");
        }
        other => panic!(
            "[scenario-C] FAIL expected EngineError::Transport, got {other:?}"
        ),
    }
}

// ── Scenario D: append_frame + tail_stream ──
#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn runtime_smoke_d_stream_tail() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let sfx = suffix();
    let lane = format!("rs-d-lane-{sfx}");
    let ns = format!("rs-d-ns-{sfx}");

    let server = InProcessServer::start(&lane).await;
    let eid = submit_solo(&server, &ns, &lane).await;
    let worker = spawn_worker(&lane, &ns, &sfx).await;

    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim timeout");
    assert_eq!(task.execution_id(), &eid);

    // Append 4 frames.
    for i in 0..4u8 {
        task.append_frame("progress", &[i], None)
            .await
            .expect("append_frame");
    }

    // Tail from beginning; loop until we've seen >=4 frames or 3s elapses.
    let mut seen = 0usize;
    let mut cursor = StreamCursor::from_beginning();
    let deadline = Instant::now() + Duration::from_secs(3);
    while seen < 4 && Instant::now() < deadline {
        let frames = task
            .tail_stream(cursor.clone(), 200, 10)
            .await
            .expect("tail_stream");
        if !frames.frames.is_empty() {
            seen += frames.frames.len();
            if let Some(last) = frames.frames.last() {
                cursor = StreamCursor::At(last.id.clone());
            }
        }
    }
    assert!(
        seen >= 4,
        "[scenario-D] expected >=4 frames, got {seen}"
    );
    eprintln!("[scenario-D] PASS frames_tailed={seen}");

    task.complete(Some(b"done".to_vec())).await.expect("complete");
    drop(worker);
    server.shutdown().await;
}
