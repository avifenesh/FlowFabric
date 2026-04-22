//! Shared helpers for the PR-D1c fanout/fanin DAG tests.
//!
//! Integration-test "common" module: each test file includes this
//! via `mod common;` + `use common::*;`. Not part of the crate's
//! public API — lives under `tests/common/mod.rs` per cargo's
//! convention for shared integration-test code (see the Rust Book
//! §Submodules in Integration Tests).
//!
//! Scope is deliberately narrow: the shape-builders and poll helpers
//! used by both `fanout_fanin_flow_happy.rs` and
//! `fanout_fanin_flow_fanin_skip.rs`. Nothing is re-exported beyond
//! what those two files need.

#![cfg(feature = "readiness")]
// Both consumer files don't use every helper. Silencing is
// preferable to per-fn `#[allow]` clutter — the module is
// tests-only and not shipped.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use ff_core::contracts::{
    AddExecutionToFlowArgs, ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::TEST_PARTITION_CONFIG;
use ff_sdk::task::ClaimedTask;

/// Diamond DAG handle: `A → {B, C} → D`, plus the flow id.
pub struct Dag {
    pub flow_id: FlowId,
    pub a: ExecutionId,
    pub b: ExecutionId,
    pub c: ExecutionId,
    pub d: ExecutionId,
}

/// Spawn an SDK worker against the lane + namespace.
pub async fn spawn_worker(lane: &str, ns: &str, suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: ff_readiness_tests::valkey::backend_config_from_env(),
        worker_id: WorkerId::new(format!("readiness-fanout-worker-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("readiness-fanout-inst-{suffix}")),
        namespace: Namespace::new(ns),
        lanes: vec![LaneId::new(lane)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 4,
    };
    ff_sdk::FlowFabricWorker::connect(cfg)
        .await
        .expect("FlowFabricWorker::connect")
}

/// Create flow + 4 members + 4 `success_only` edges (A→B, A→C, B→D,
/// C→D). After `stage_dependency_edge` bumps graph_revision per edge,
/// `apply_dependency_to_child` stamps the dep onto the downstream and
/// marks it WaitingChildren. A starts Waiting (no incoming edges).
pub async fn stage_diamond_dag(server: &InProcessServer, ns: &str, lane: &str) -> Dag {
    let flow_id = FlowId::new();
    let now = TimestampMs::now();

    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "readiness-fanout".to_owned(),
            namespace: Namespace::new(ns),
            now,
        })
        .await
        .expect("create_flow");

    let a = create_member(server, &flow_id, ns, lane, "A", now).await;
    let b = create_member(server, &flow_id, ns, lane, "B", now).await;
    let c = create_member(server, &flow_id, ns, lane, "C", now).await;
    let d = create_member(server, &flow_id, ns, lane, "D", now).await;

    // ff_add_execution_to_flow bumps graph_revision by 1 per member,
    // so the first staged edge expects revision 4. After that, thread
    // the authoritative new_graph_revision returned by
    // stage_dependency_edge through each subsequent call — assuming a
    // fixed +1 bump is brittle against future Lua changes.
    let mut rev: u64 = 4;
    let (e_ab, new_rev) = stage_and_apply_edge(server, &flow_id, &a, &b, rev).await;
    rev = new_rev;
    let (e_ac, new_rev) = stage_and_apply_edge(server, &flow_id, &a, &c, rev).await;
    rev = new_rev;
    let (e_bd, new_rev) = stage_and_apply_edge(server, &flow_id, &b, &d, rev).await;
    rev = new_rev;
    let (e_cd, _final_rev) = stage_and_apply_edge(server, &flow_id, &c, &d, rev).await;

    // Edge ids are recoverable at read-time via list_*_edges; not
    // load-bearing for assertions, so don't stash them.
    let _ = (e_ab, e_ac, e_bd, e_cd);

    Dag { flow_id, a, b, c, d }
}

async fn create_member(
    server: &InProcessServer,
    flow_id: &FlowId,
    ns: &str,
    lane: &str,
    tag: &str,
    now: TimestampMs,
) -> ExecutionId {
    let eid = ExecutionId::for_flow(flow_id, &TEST_PARTITION_CONFIG);
    let deadline_at = TimestampMs::from_millis(now.0 + 60_000);
    server
        .server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(ns),
            lane_id: LaneId::new(lane),
            execution_kind: format!("readiness-fanout-{tag}"),
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
    eid
}

/// Stage one `success_only` edge + apply it to the downstream. Both
/// ops always happen together in this test, so one helper. Returns
/// `(edge_id, new_graph_revision)` so callers thread the server's
/// authoritative revision into the next stage call rather than
/// assuming a fixed +1 bump.
async fn stage_and_apply_edge(
    server: &InProcessServer,
    flow_id: &FlowId,
    up: &ExecutionId,
    down: &ExecutionId,
    expected_graph_revision: u64,
) -> (EdgeId, u64) {
    let edge_id = EdgeId::new();
    let now = TimestampMs::now();
    let staged = server
        .server
        .stage_dependency_edge(&StageDependencyEdgeArgs {
            flow_id: flow_id.clone(),
            edge_id: edge_id.clone(),
            upstream_execution_id: up.clone(),
            downstream_execution_id: down.clone(),
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            expected_graph_revision,
            now,
        })
        .await
        .expect("stage_dependency_edge");
    let new_rev = match staged {
        StageDependencyEdgeResult::Staged { new_graph_revision, .. } => new_graph_revision,
    };
    server
        .server
        .apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: flow_id.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: down.clone(),
            upstream_execution_id: up.clone(),
            graph_revision: new_rev,
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            now,
        })
        .await
        .expect("apply_dependency_to_child");
    (edge_id, new_rev)
}

/// Poll `worker.claim_next` until the reclaimed task matches `eid`.
/// Mirrors waitpoint_hmac_roundtrip's reclaim helper but scoped local.
/// Claim the single eligible task on the worker's lane, matching
/// `eid`. Panics if the next claim returns a different task — per
/// caller invariant, only `eid` is expected to be eligible. If the
/// test exposes concurrently-eligible executions (e.g. a fanout), use
/// [`claim_any_of`] instead; dropping a wrongly-claimed task holds its
/// 30 s lease and wedges subsequent `claim_specific(other_eid)` calls.
pub async fn claim_specific(
    worker: &ff_sdk::FlowFabricWorker,
    eid: &ExecutionId,
) -> ClaimedTask {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                assert_eq!(
                    t.execution_id(),
                    eid,
                    "claim_specific invariant violated: expected sole-eligible \
                     {eid}, got {}. Another execution is concurrently eligible — \
                     use claim_any_of() instead.",
                    t.execution_id()
                );
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("claim_specific timed out waiting for {eid}"))
}

/// Claim whichever of `eids` the worker returns next. Panics if the
/// claim is none of them. Used when the test drives a fanout where
/// multiple downstreams become eligible in parallel and the caller
/// wants to iterate through them in worker-claim order.
///
/// Returns `(idx, task)` where `idx` is the position in `eids` that
/// matched, so callers can update their "remaining" set.
pub async fn claim_any_of(
    worker: &ff_sdk::FlowFabricWorker,
    eids: &[&ExecutionId],
) -> (usize, ClaimedTask) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                let got = t.execution_id().clone();
                match eids.iter().position(|e| *e == &got) {
                    Some(i) => break (i, t),
                    None => panic!(
                        "claim_any_of: worker returned {got} which is not in \
                         expected set {eids:?}"
                    ),
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("claim_any_of timed out waiting for one of {eids:?}"))
}

/// One-shot public_state read.
pub async fn read_public_state(
    worker: &ff_sdk::FlowFabricWorker,
    eid: &ExecutionId,
) -> PublicState {
    worker
        .describe_execution(eid)
        .await
        .expect("describe_execution")
        .unwrap_or_else(|| panic!("snapshot missing for {eid}"))
        .public_state
}

/// Assert single-shot: current public_state matches.
pub async fn assert_public_state(
    worker: &ff_sdk::FlowFabricWorker,
    eid: &ExecutionId,
    expected: PublicState,
) {
    let got = read_public_state(worker, eid).await;
    assert_eq!(
        got, expected,
        "{eid} expected public_state={expected:?}, got {got:?}"
    );
}

/// Bounded poll until public_state becomes `expected`. Fails with a
/// descriptive message on timeout so a push-promotion regression
/// fails fast rather than hanging the ignored run.
pub async fn await_public_state(
    worker: &ff_sdk::FlowFabricWorker,
    eid: &ExecutionId,
    expected: PublicState,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = read_public_state(worker, eid).await;
        if got == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {eid} to become public_state={expected:?} (5s); \
                 last public_state={got:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Invariant helper: for `ms` milliseconds (sampled at 100 ms cadence),
/// the execution's `public_state` must NOT become `Waiting` (i.e. must
/// NOT be eligible-to-claim). If it does, that is a fanin-semantics
/// violation — an `all_required` downstream was promoted before ALL of
/// its parents reached a satisfying terminal state.
///
/// Single-shot checks against an async completion listener are racey,
/// so we sample across the full window. Uses an `Instant`-based
/// deadline so the window covers exactly `ms` milliseconds even for
/// `ms` values that aren't multiples of 100 (e.g. 150 ms previously
/// floored to a single sample via integer division).
pub async fn assert_not_waiting_for(
    worker: &ff_sdk::FlowFabricWorker,
    eid: &ExecutionId,
    ms: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut sample = 0usize;
    loop {
        let got = read_public_state(worker, eid).await;
        assert_ne!(
            got,
            PublicState::Waiting,
            "{eid} became Waiting at sample {sample} within {ms}ms window \
             — fanin `all_required` violated (downstream promoted before \
             all parents terminal); public_state={got:?}"
        );
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        sample += 1;
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
}
