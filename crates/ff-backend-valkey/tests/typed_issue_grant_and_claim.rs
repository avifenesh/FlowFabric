//! cairn #454 Phase 3d — live-Valkey coverage of
//! `EngineBackend::issue_grant_and_claim`.
//!
//! Asserts the round-trip through
//! `ValkeyBackend::issue_grant_and_claim` →
//! `ff_issue_grant_and_claim` FCALL → `ClaimGrantOutcome` for:
//!
//! 1. Happy path — runnable+unowned+eligible execution transitions to
//!    `active/leased` in one FCALL, returns `{lease_id, epoch=1,
//!    attempt_index=0}`.
//! 2. Execution already leased returns a typed reject (lease_conflict).
//! 3. Non-existent execution → `EngineError::Validation` translating
//!    from Lua `execution_not_found` (no exec_core means
//!    `NotFound`-shaped reject).
//! 4. Atomicity — because the whole op lives inside ONE Lua FCALL
//!    (single-Valkey-command-serial), there is no intermediate state
//!    a caller can observe between `issue_claim_grant` and the claim.
//!    A grant either is written+consumed atomically with the claim,
//!    or neither side happens. The structural proof lives in
//!    `lua/scheduling.lua::ff_issue_grant_and_claim`; this test
//!    pins the post-success state (grant key absent, lease present).
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379). Run via:
//!
//! ```text
//! cargo test -p ff-backend-valkey --test typed_issue_grant_and_claim \
//!     -- --ignored --test-threads=1
//! ```

#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::contracts::IssueGrantAndClaimArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, execution_partition};
use ff_core::types::{ExecutionId, FlowId, LaneId};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 6379;
const LANE: &str = "issue-grant-and-claim-test-lane";
const NS: &str = "cairn-454-phase-3d";

fn cfg() -> PartitionConfig {
    PartitionConfig::default()
}

fn new_eid() -> ExecutionId {
    ExecutionId::for_flow(&FlowId::new(), &cfg())
}

fn partition_for(eid: &ExecutionId) -> Partition {
    execution_partition(eid, &cfg())
}

async fn raw_client() -> ferriskey::Client {
    let client = ferriskey::ClientBuilder::new()
        .host(HOST, PORT)
        .build()
        .await
        .expect("build raw ferriskey client");
    // Load (or reload on version bump) the flowfabric Lua library so
    // raw FCALLs in `seed_runnable` see the fresh
    // `ff_issue_grant_and_claim` registration.
    ff_script::loader::ensure_library(&client)
        .await
        .expect("ensure_library");
    client
}

async fn connect_backend() -> Arc<dyn EngineBackend> {
    ValkeyBackend::connect(BackendConfig::valkey(HOST, PORT))
        .await
        .expect("connect ValkeyBackend")
}

/// Minimal `ff_create_execution` seed — mirrors `rfc024_reclaim`'s helper.
/// Leaves execution in `runnable / unowned / eligible_now` state.
async fn seed_runnable(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

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
        "standalone".to_owned(),
        "0".to_owned(),
        "cairn-454-phase-3d-test".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        "0".to_owned(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = client
        .fcall("ff_create_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_execution");
}

async fn hget_core(client: &ferriskey::Client, eid: &ExecutionId, field: &str) -> Option<String> {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    client
        .cmd("HGET")
        .arg(ctx.core())
        .arg(field)
        .execute()
        .await
        .expect("HGET exec_core field")
}

async fn exists(client: &ferriskey::Client, key: &str) -> bool {
    let n: i64 = client
        .cmd("EXISTS")
        .arg(key)
        .execute()
        .await
        .expect("EXISTS");
    n == 1
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn happy_path_grants_and_claims_in_one_fcall() {
    let raw = raw_client().await;
    let backend = connect_backend().await;
    let eid = new_eid();
    seed_runnable(&raw, &eid).await;

    let outcome = backend
        .issue_grant_and_claim(IssueGrantAndClaimArgs::new(
            eid.clone(),
            LaneId::new(LANE),
            30_000,
        ))
        .await
        .expect("issue_grant_and_claim happy path");

    // Lease surface: epoch flips 0 → 1 and attempt_index starts at 0.
    assert_eq!(outcome.lease_epoch.0, 1);
    assert_eq!(outcome.attempt_index.0, 0);
    assert!(
        !outcome.lease_id.to_string().is_empty(),
        "lease_id must be populated"
    );

    // exec_core reflects the claim: active / leased / current lease id
    // matches what we got back.
    assert_eq!(
        hget_core(&raw, &eid, "lifecycle_phase").await.as_deref(),
        Some("active")
    );
    assert_eq!(
        hget_core(&raw, &eid, "ownership_state").await.as_deref(),
        Some("leased")
    );
    assert_eq!(
        hget_core(&raw, &eid, "current_lease_id").await.as_deref(),
        Some(outcome.lease_id.to_string().as_str())
    );
    assert_eq!(
        hget_core(&raw, &eid, "current_worker_id").await.as_deref(),
        Some("operator")
    );

    // Grant key is DEL'd — the atomic FCALL consumed it.
    let partition = partition_for(&eid);
    let ctx = ExecKeyContext::new(&partition, &eid);
    assert!(
        !exists(&raw, &ctx.claim_grant()).await,
        "claim_grant must be DEL'd after the fused FCALL (atomic proof: \
         no intermediate observable state)"
    );

    // Lease record written.
    assert!(
        exists(&raw, &ctx.lease_current()).await,
        "lease_current hash must exist post-claim"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn second_call_on_same_execution_is_rejected() {
    let raw = raw_client().await;
    let backend = connect_backend().await;
    let eid = new_eid();
    seed_runnable(&raw, &eid).await;

    // First call — succeeds, execution is now active/leased.
    let _ = backend
        .issue_grant_and_claim(IssueGrantAndClaimArgs::new(
            eid.clone(),
            LaneId::new(LANE),
            30_000,
        ))
        .await
        .expect("first issue_grant_and_claim succeeds");

    // Second call — exec is no longer runnable; Lua returns
    // `execution_not_eligible`.
    let err = backend
        .issue_grant_and_claim(IssueGrantAndClaimArgs::new(
            eid.clone(),
            LaneId::new(LANE),
            30_000,
        ))
        .await
        .expect_err("second issue_grant_and_claim on already-leased exec must error");

    // Typed reject shape: we only care that it's a typed EngineError
    // (not a Transport crash). The specific variant is whatever
    // `From<ScriptError> for EngineError` maps `execution_not_eligible`
    // to — document the shape as a snapshot so future regressions
    // surface.
    match err {
        EngineError::Validation { .. }
        | EngineError::State { .. }
        | EngineError::Contention(_) => {}
        other => panic!(
            "expected Validation/State/Contention reject on already-leased exec, got {other:?}"
        ),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn non_existent_execution_is_rejected() {
    let backend = connect_backend().await;
    // Do NOT seed — exec_core missing.
    let eid = new_eid();

    let err = backend
        .issue_grant_and_claim(IssueGrantAndClaimArgs::new(
            eid,
            LaneId::new(LANE),
            30_000,
        ))
        .await
        .expect_err("issue_grant_and_claim on missing exec must error");

    // Maps from Lua `execution_not_found`. Accept any typed variant —
    // the important invariant is no panic + no transport fault.
    match err {
        EngineError::NotFound { .. }
        | EngineError::Validation { .. }
        | EngineError::State { .. } => {}
        other => panic!("expected typed reject on missing exec, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn fused_fcall_leaves_no_dangling_grant() {
    // Atomicity proof (structural). The composed Lua lives inside a
    // single FCALL → single-Valkey-command-serial → no caller can
    // observe a state where `issue_claim_grant` wrote the grant hash
    // but `claim_execution` failed. We pin the post-success invariant
    // here; fault-injection mid-FCALL isn't expressible against a
    // real Valkey server (Lua is atomic by construction).
    let raw = raw_client().await;
    let backend = connect_backend().await;
    let eid = new_eid();
    seed_runnable(&raw, &eid).await;

    let _ = backend
        .issue_grant_and_claim(IssueGrantAndClaimArgs::new(
            eid.clone(),
            LaneId::new(LANE),
            30_000,
        ))
        .await
        .expect("issue_grant_and_claim succeeds");

    let partition = partition_for(&eid);
    let ctx = ExecKeyContext::new(&partition, &eid);

    // Post-success: grant key absent, lease present. These two
    // together are the atomicity witness — any observable state
    // where the grant exists without a lease would indicate the Lua
    // broke its single-command contract.
    assert!(
        !exists(&raw, &ctx.claim_grant()).await,
        "grant key must not persist past the fused FCALL"
    );
    assert!(
        exists(&raw, &ctx.lease_current()).await,
        "lease_current must be written atomically with the grant consumption"
    );
}
