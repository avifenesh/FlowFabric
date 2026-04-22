//! PR-D1 / Test 2 — `cancel_flow_partial` regression proof.
//!
//! Drives `cancel_flow` through the HTTP surface (`POST
//! /v1/flows/{id}/cancel?wait=true`) to prove the PR-C1 fix holds at
//! the API layer — not just at the FCALL / typed-Server layer that
//! Worker C's `pr_c1_cancel_flow_partial` already covers.
//!
//! Failure injection mirrors C1's pattern: two real member executions,
//! the second flipped to lifecycle_phase=active with its
//! `attempt_hash` planted as a STRING so the in-Lua `HSET` returns
//! WRONGTYPE. That bubbles out as a non-terminal-ack error, and the
//! `?wait=true` branch is obliged to report
//! `CancelFlowResult::PartiallyCancelled` with the sabotaged id.
//!
//! RED-PROOF: pre-PR-C1 (commit cd61f11^) returned
//! `CancelFlowResult::Cancelled` even when one member's cancel failed.
//! This test pins the post-fix contract. Verifiable by
//! `git show cd61f11^:crates/ff-server/src/server.rs` around line
//! 1598. No local red-proof log — reverting the fix in-tree to capture
//! a RED run would contaminate the branch (per owner's prior note).
//! The PR body cites `cd61f11` as the regression anchor.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test cancel_flow_partial -- --ignored --test-threads=1
#![cfg(feature = "readiness")]

use std::time::Instant;

use ff_core::contracts::{
    AddExecutionToFlowArgs, CancelFlowArgs, CancelFlowResult, CreateExecutionArgs, CreateFlowArgs,
};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster, TEST_PARTITION_CONFIG};
use serde_json::json;

const TEST_NAME: &str = "cancel_flow_partial";
const LANE: &str = "readiness-cancel";
const NS: &str = "readiness-cancel-ns";

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn cancel_flow_partial() {
    let t_start = Instant::now();

    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;

    let server = InProcessServer::start(LANE).await;

    // ── Sabotaged path: one clean member + one wrong-type member ──
    let flow_id = FlowId::new();
    let (member_ids, broken_eid) = create_flow_with_two_members(&server, &flow_id).await;
    sabotage_member_attempt_hash(&tc, &broken_eid).await;

    // Per-request timeout bounds stalled connects/reads so the
    // ignored readiness run fails fast on regression, not hang.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("build reqwest client");
    let url = format!("{}/v1/flows/{}/cancel?wait=true", server.base_url, flow_id);
    let resp = http
        .post(&url)
        .json(&CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "readiness-cancel".to_owned(),
            cancellation_policy: "cancel_all".to_owned(),
            now: TimestampMs::now(),
        })
        .send()
        .await
        .expect("HTTP POST cancel?wait=true");
    assert!(
        resp.status().is_success(),
        "cancel_flow?wait=true returned {}",
        resp.status()
    );
    let result: CancelFlowResult = resp.json().await.expect("deserialize CancelFlowResult");

    match &result {
        CancelFlowResult::PartiallyCancelled {
            cancellation_policy,
            member_execution_ids,
            failed_member_execution_ids,
        } => {
            assert_eq!(cancellation_policy, "cancel_all");
            assert_eq!(
                member_execution_ids.len(),
                2,
                "both members should be listed in the full membership"
            );
            assert_eq!(
                failed_member_execution_ids.len(),
                1,
                "exactly one member should have failed to cancel"
            );
            assert_eq!(
                failed_member_execution_ids[0],
                broken_eid.to_string(),
                "sabotaged member should be the failed one"
            );
        }
        other => panic!(
            "sabotaged cancel should return PartiallyCancelled, got {other:?}"
        ),
    }

    let sabotaged_members: Vec<String> = member_ids.iter().map(|e| e.to_string()).collect();

    // ── Control path: same shape, no sabotage, expect Cancelled ──
    let control_flow_id = FlowId::new();
    let (control_members, _) = create_flow_with_two_members(&server, &control_flow_id).await;
    let control_url = format!(
        "{}/v1/flows/{}/cancel?wait=true",
        server.base_url, control_flow_id
    );
    let control_resp = http
        .post(&control_url)
        .json(&CancelFlowArgs {
            flow_id: control_flow_id.clone(),
            reason: "readiness-cancel-control".to_owned(),
            cancellation_policy: "cancel_all".to_owned(),
            now: TimestampMs::now(),
        })
        .send()
        .await
        .expect("HTTP POST cancel (control)");
    assert!(
        control_resp.status().is_success(),
        "control cancel returned {}",
        control_resp.status()
    );
    let control_result: CancelFlowResult = control_resp
        .json()
        .await
        .expect("deserialize control CancelFlowResult");

    match &control_result {
        CancelFlowResult::Cancelled {
            cancellation_policy,
            member_execution_ids,
        } => {
            assert_eq!(cancellation_policy, "cancel_all");
            assert_eq!(
                member_execution_ids.len(),
                2,
                "control flow should report both members cancelled"
            );
        }
        other => panic!(
            "control cancel should return Cancelled (no sabotage), got {other:?}"
        ),
    }

    // Explicit teardown — drains engine + background tasks so state
    // does not leak into the next serial readiness test.
    server.shutdown().await;

    evidence::write(
        TEST_NAME,
        &json!({
            "test": TEST_NAME,
            "total_ms": t_start.elapsed().as_millis() as u64,
            "valkey_version": valkey_version,
            "sabotaged": {
                "flow_id": flow_id.to_string(),
                "member_ids": sabotaged_members,
                "broken_member_id": broken_eid.to_string(),
                "result_variant": "PartiallyCancelled",
            },
            "control": {
                "flow_id": control_flow_id.to_string(),
                "member_ids": control_members.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
                "result_variant": "Cancelled",
            },
            "regression_anchor": {
                "commit": "cd61f11",
                "note": "pre-fix returned Cancelled on partial failure",
            },
        }),
    );
}

/// Create a flow plus two real member executions and return their ids
/// (second id is the one callers sabotage for the partial-cancel path).
async fn create_flow_with_two_members(
    server: &InProcessServer,
    flow_id: &FlowId,
) -> (Vec<ExecutionId>, ExecutionId) {
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "readiness-cancel".to_owned(),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow");

    let a = ExecutionId::for_flow(flow_id, &TEST_PARTITION_CONFIG);
    let b = ExecutionId::for_flow(flow_id, &TEST_PARTITION_CONFIG);
    assert_ne!(a, b, "member ids must be distinct");

    for eid in [&a, &b] {
        server
            .server
            .create_execution(&CreateExecutionArgs {
                execution_id: eid.clone(),
                namespace: Namespace::new(NS),
                lane_id: LaneId::new(LANE),
                execution_kind: "readiness-cancel-member".to_owned(),
                input_payload: b"{}".to_vec(),
                payload_encoding: Some("json".to_owned()),
                priority: 0,
                creator_identity: "readiness".to_owned(),
                idempotency_key: None,
                tags: std::collections::HashMap::new(),
                policy: None,
                delay_until: None,
                execution_deadline_at: None,
                partition_id: execution_partition(eid, &TEST_PARTITION_CONFIG).index,
                now,
            })
            .await
            .expect("create_execution member");
        server
            .server
            .add_execution_to_flow(&AddExecutionToFlowArgs {
                flow_id: flow_id.clone(),
                execution_id: eid.clone(),
                now,
            })
            .await
            .expect("add_execution_to_flow member");
    }

    (vec![a.clone(), b.clone()], b)
}

/// Flip the execution to lifecycle_phase="active" + plant its
/// attempt_hash as a STRING. The cancel Lua's `HSET K.attempt_hash …`
/// returns WRONGTYPE, which bubbles out as a non-ack failure.
async fn sabotage_member_attempt_hash(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, &TEST_PARTITION_CONFIG);
    let ctx = ExecKeyContext::new(&partition, eid);
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("lifecycle_phase")
        .arg("active")
        .arg("ownership_state")
        .arg("leased")
        .arg("current_attempt_index")
        .arg("1")
        .execute()
        .await
        .expect("HSET broken core active");

    let attempt_key = ctx.attempt_hash(AttemptIndex::new(1));
    let _: String = tc
        .client()
        .cmd("SET")
        .arg(attempt_key.as_str())
        .arg("wrongtype-sentinel")
        .execute()
        .await
        .expect("SET attempt_hash as string");
}
