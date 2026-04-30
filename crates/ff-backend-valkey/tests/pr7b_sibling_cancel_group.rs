//! PR-7b Cluster 3 — direct trait integration for
//! `EngineBackend::drain_sibling_cancel_group` +
//! `EngineBackend::reconcile_sibling_cancel_group` on Valkey.
//!
//! These tests complement the full-server RFC-016 Stage D scenarios in
//! `ff-test::flow_edge_policies_stage_d` by exercising the new trait
//! methods (PR-7b/3) against a live Valkey without a running engine.
//! Keeps the trait shim honest against the underlying
//! `ff_drain_sibling_cancel_group` / `ff_reconcile_sibling_cancel_group`
//! Lua functions.
//!
//! Gated `#[ignore]` because they require a live Valkey at
//! `127.0.0.1:6379` (same convention as the sibling `rfc024_reclaim`
//! suite). Run with:
//!
//! ```
//! valkey-cli -h 127.0.0.1 -p 6379 ping
//! cargo test -p ff-backend-valkey --test pr7b_sibling_cancel_group -- --ignored
//! ```

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::engine_backend::{EngineBackend, SiblingCancelReconcileAction};
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition, PartitionConfig};
use ff_core::types::{ExecutionId, FlowId};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 6379;

fn cfg() -> PartitionConfig {
    PartitionConfig::default()
}

async fn raw_client() -> ferriskey::Client {
    ferriskey::ClientBuilder::new()
        .host(HOST, PORT)
        .build()
        .await
        .expect("build raw ferriskey client")
}

async fn connect_backend() -> Arc<dyn EngineBackend> {
    ValkeyBackend::connect(BackendConfig::valkey(HOST, PORT))
        .await
        .expect("connect ValkeyBackend")
}

/// SADD the pending-cancel-groups tuple the way the dispatcher Lua would
/// have before crashing.
async fn sadd_pending(client: &ferriskey::Client, fid: &FlowId, downstream: &ExecutionId) {
    let fp = flow_partition(fid, &cfg());
    let pending_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let member = format!("{}|{}", fid, downstream);
    let _: i64 = client
        .cmd("SADD")
        .arg(pending_key.as_str())
        .arg(member.as_str())
        .execute()
        .await
        .expect("SADD pending_cancel_groups");
}

async fn sismember_pending(
    client: &ferriskey::Client,
    fid: &FlowId,
    downstream: &ExecutionId,
) -> bool {
    let fp = flow_partition(fid, &cfg());
    let pending_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let member = format!("{}|{}", fid, downstream);
    client
        .cmd("SISMEMBER")
        .arg(pending_key.as_str())
        .arg(member.as_str())
        .execute()
        .await
        .expect("SISMEMBER pending_cancel_groups")
}

/// Seed a satisfied edgegroup hash. `flag=true` → dispatcher still owes
/// drain; `flag=false` → dispatcher already HDEL'd flag but crashed
/// before the SREM.
async fn seed_edgegroup(
    client: &ferriskey::Client,
    fid: &FlowId,
    downstream: &ExecutionId,
    siblings: &[ExecutionId],
    flag: bool,
) {
    let fp = flow_partition(fid, &cfg());
    let fctx = FlowKeyContext::new(&fp, fid);
    let edgegroup_key = fctx.edgegroup(downstream);
    let members_str: String = siblings
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("|");

    let cmd = client
        .cmd("HSET")
        .arg(edgegroup_key.as_str())
        .arg("policy_variant")
        .arg("any_of")
        .arg("on_satisfied")
        .arg("cancel_remaining")
        .arg("group_state")
        .arg("satisfied")
        .arg("cancel_siblings_reason")
        .arg("sibling_quorum_satisfied")
        .arg("cancel_siblings_pending_members")
        .arg(members_str.as_str());
    let cmd = if flag {
        cmd.arg("cancel_siblings_pending_flag").arg("true")
    } else {
        cmd
    };
    let _: i64 = cmd.execute().await.expect("HSET edgegroup");
}

/// Force an exec_core into terminal/cancelled so the reconciler's
/// terminal-check sees it as already drained.
async fn force_terminal(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = execution_partition(eid, &cfg());
    let ctx = ExecKeyContext::new(&partition, eid);
    let _: i64 = client
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("lifecycle_phase")
        .arg("terminal")
        .arg("terminal_outcome")
        .arg("cancelled")
        .arg("cancellation_reason")
        .arg("sibling_quorum_satisfied")
        .execute()
        .await
        .expect("HSET exec_core terminal");
}

/// Cleanup keys between test runs so repeated `--ignored` invocations
/// don't interfere.
async fn cleanup(client: &ferriskey::Client, fid: &FlowId, downstream: &ExecutionId, siblings: &[ExecutionId]) {
    let fp = flow_partition(fid, &cfg());
    let fctx = FlowKeyContext::new(&fp, fid);
    let fidx = FlowIndexKeys::new(&fp);
    let _: i64 = client
        .cmd("DEL")
        .arg(fctx.edgegroup(downstream).as_str())
        .arg(fidx.pending_cancel_groups().as_str())
        .execute()
        .await
        .unwrap_or(0);
    for eid in siblings {
        let partition = execution_partition(eid, &cfg());
        let ctx = ExecKeyContext::new(&partition, eid);
        let _: i64 = client
            .cmd("DEL")
            .arg(ctx.core().as_str())
            .execute()
            .await
            .unwrap_or(0);
    }
}

fn mint_sibling_ids(n: usize) -> Vec<ExecutionId> {
    (0..n).map(|_| ExecutionId::for_flow(&FlowId::new(), &cfg())).collect()
}

/// `drain_sibling_cancel_group` on a satisfied group with flag set +
/// members already terminal drains the flag and removes the tuple.
/// Mirrors the dispatcher's own drain step but exercised through the
/// trait.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn drain_sibling_cancel_group_removes_tuple() {
    let backend = connect_backend().await;
    let raw = raw_client().await;
    let fid = FlowId::new();
    let downstream = ExecutionId::for_flow(&FlowId::new(), &cfg());
    let siblings = mint_sibling_ids(2);
    cleanup(&raw, &fid, &downstream, &siblings).await;

    // Seed: flag=true, members terminal, pending tuple present.
    seed_edgegroup(&raw, &fid, &downstream, &siblings, true).await;
    for s in &siblings {
        force_terminal(&raw, s).await;
    }
    sadd_pending(&raw, &fid, &downstream).await;
    assert!(sismember_pending(&raw, &fid, &downstream).await);

    // Exercise trait method.
    let fp = flow_partition(&fid, &cfg());
    backend
        .drain_sibling_cancel_group(fp, &fid, &downstream)
        .await
        .expect("drain_sibling_cancel_group");

    // Post-condition: pending tuple gone.
    assert!(!sismember_pending(&raw, &fid, &downstream).await);

    cleanup(&raw, &fid, &downstream, &siblings).await;
}

/// `reconcile_sibling_cancel_group` on a stale tuple (flag absent, no
/// edgegroup hash) SREMs the tuple and reports `SremmedStale`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reconcile_sibling_cancel_group_sremms_stale() {
    let backend = connect_backend().await;
    let raw = raw_client().await;
    let fid = FlowId::new();
    let downstream = ExecutionId::for_flow(&FlowId::new(), &cfg());
    let siblings: Vec<ExecutionId> = Vec::new();
    cleanup(&raw, &fid, &downstream, &siblings).await;

    // Seed: pending tuple with no edgegroup hash at all — the canonical
    // stale shape.
    sadd_pending(&raw, &fid, &downstream).await;
    assert!(sismember_pending(&raw, &fid, &downstream).await);

    let fp = flow_partition(&fid, &cfg());
    let action = backend
        .reconcile_sibling_cancel_group(fp, &fid, &downstream)
        .await
        .expect("reconcile_sibling_cancel_group");

    assert_eq!(action, SiblingCancelReconcileAction::SremmedStale);
    assert!(!sismember_pending(&raw, &fid, &downstream).await);

    cleanup(&raw, &fid, &downstream, &siblings).await;
}

/// `reconcile_sibling_cancel_group` on an interrupted drain (flag=true,
/// every listed sibling already terminal) returns `CompletedDrain` and
/// clears the pending tuple.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reconcile_sibling_cancel_group_completes_drain() {
    let backend = connect_backend().await;
    let raw = raw_client().await;
    let fid = FlowId::new();
    let downstream = ExecutionId::for_flow(&FlowId::new(), &cfg());
    let siblings = mint_sibling_ids(2);
    cleanup(&raw, &fid, &downstream, &siblings).await;

    seed_edgegroup(&raw, &fid, &downstream, &siblings, true).await;
    for s in &siblings {
        force_terminal(&raw, s).await;
    }
    sadd_pending(&raw, &fid, &downstream).await;

    let fp = flow_partition(&fid, &cfg());
    let action = backend
        .reconcile_sibling_cancel_group(fp, &fid, &downstream)
        .await
        .expect("reconcile_sibling_cancel_group");

    assert_eq!(action, SiblingCancelReconcileAction::CompletedDrain);
    assert!(!sismember_pending(&raw, &fid, &downstream).await);

    cleanup(&raw, &fid, &downstream, &siblings).await;
}
