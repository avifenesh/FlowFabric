//! Cairn #454 Phase 3c — `EngineBackend::deliver_approval_signal` on
//! the Valkey backend.
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `127.0.0.1:6379`. Run with:
//!
//! ```
//! valkey-cli -h 127.0.0.1 -p 6379 ping
//! cargo test -p ff-backend-valkey --test typed_deliver_approval_signal \
//!   -- --ignored --test-threads=1
//! ```
//!
//! Shape mirrors `rfc024_reclaim.rs`: each test bootstraps a live
//! execution via raw FCALLs (create → issue_claim_grant → claim) up to
//! a `HandleKind::Fresh` handle, rotates a kid into
//! `ff_waitpoint_hmac` (mandatory for `suspend` to mint a token), then
//! calls `backend.suspend(...)` via the trait to land a waitpoint row
//! and exercises `deliver_approval_signal`.

#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{BackendConfig, Handle};
use ff_core::contracts::{
    CompositeBody, CountKind, DeliverApprovalSignalArgs, DeliverSignalResult,
    ResumeCondition, ResumePolicy, RotateWaitpointHmacSecretAllArgs, SignalMatcher,
    SuspendArgs, SuspendOutcome, SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, execution_partition};
use ff_core::types::{
    AttemptIndex, ExecutionId, FlowId, LaneId, SuspensionId, TimestampMs, WaitpointId,
    WorkerInstanceId,
};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 6379;
const LANE: &str = "default";
const WORKER: &str = "approval-worker";
const WORKER_INST: &str = "approval-worker-inst";
const NS: &str = "approval-signal-ns";

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

/// Pin `ff:config:partitions` to `PartitionConfig::default()` so the
/// backend's routing aligns with the test's key derivation. Mirrors
/// the helper in `rfc024_reclaim.rs`.
async fn ensure_default_partition_config(client: &ferriskey::Client) {
    let key = ff_core::keys::global_config_partitions();
    let default = PartitionConfig::default();
    let _: Value = client
        .cmd("HSET")
        .arg(&key)
        .arg("num_flow_partitions")
        .arg(default.num_flow_partitions.to_string())
        .arg("num_budget_partitions")
        .arg(default.num_budget_partitions.to_string())
        .arg("num_quota_partitions")
        .arg(default.num_quota_partitions.to_string())
        .execute()
        .await
        .unwrap_or(Value::Nil);
}

async fn ensure_hmac_secret(backend: &Arc<dyn EngineBackend>) {
    // Replay-safe: repeated same (kid, secret) rotate is Noop per
    // `ff_rotate_waitpoint_hmac_secret`.
    let args = RotateWaitpointHmacSecretAllArgs::new(
        "approval-kid-v1",
        "ab".repeat(32),
        0,
    );
    let _ = backend
        .rotate_waitpoint_hmac_secret_all(args)
        .await
        .expect("rotate_waitpoint_hmac_secret_all");
}

async fn fcall_create_execution(client: &ferriskey::Client, eid: &ExecutionId) {
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
        "approval-test".to_owned(),
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

async fn fcall_issue_claim_grant(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let keys: Vec<String> = vec![ctx.core(), ctx.claim_grant(), idx.lane_eligible(&lane_id)];
    let args: Vec<String> = vec![
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
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = client
        .fcall("ff_issue_claim_grant", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_issue_claim_grant");
}

async fn backend_claim(backend: &Arc<dyn EngineBackend>, eid: &ExecutionId) -> Handle {
    use ff_core::contracts::{ClaimExecutionArgs, ClaimExecutionResult};
    use ff_core::types::{AttemptId, LeaseId, WorkerId};

    let args = ClaimExecutionArgs::new(
        eid.clone(),
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        LaneId::new(LANE),
        LeaseId::new(),
        30_000,
        AttemptId::new(),
        AttemptIndex::new(0),
        "{}".to_owned(),
        None,
        None,
        TimestampMs::now(),
    );
    let out = backend.claim_execution(args).await.expect("claim_execution");
    match out {
        ClaimExecutionResult::Claimed(c) => c.handle,
        other => panic!("expected Claimed, got {other:?}"),
    }
}

async fn cleanup_execution(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    for k in [
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        ctx.claim_grant(),
        ctx.lease_current(),
        ctx.lease_history(),
        ctx.attempts(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.attempt_usage(AttemptIndex::new(0)),
        ctx.attempt_policy(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.suspension_current(),
        ctx.exec_signals(),
    ] {
        let _: Value = client.cmd("DEL").arg(k).execute().await.unwrap_or(Value::Nil);
    }

    for (cmd, key) in [
        ("ZREM", idx.lease_expiry()),
        ("ZREM", idx.lane_active(&lane_id)),
        ("ZREM", idx.lane_eligible(&lane_id)),
        ("ZREM", idx.lane_terminal(&lane_id)),
        ("ZREM", idx.lane_suspended(&lane_id)),
        ("ZREM", idx.lane_delayed(&lane_id)),
        ("ZREM", idx.attempt_timeout()),
        ("ZREM", idx.execution_deadline()),
        ("ZREM", idx.all_executions()),
        ("ZREM", idx.suspension_timeout()),
        ("SREM", idx.worker_leases(&wid)),
    ] {
        let _: Value = client
            .cmd(cmd)
            .arg(key)
            .arg(eid.to_string())
            .execute()
            .await
            .unwrap_or(Value::Nil);
    }
}

/// Bootstrap: fresh execution → claim → suspend on `Single{ByName("approved")}`.
/// Returns the claimed handle + the minted waitpoint id so a subsequent
/// `deliver_approval_signal` call can target it.
async fn setup_suspended(
    backend: &Arc<dyn EngineBackend>,
    raw: &ferriskey::Client,
) -> (ExecutionId, Handle, WaitpointId) {
    ensure_default_partition_config(raw).await;
    ensure_hmac_secret(backend).await;

    let eid = new_eid();
    cleanup_execution(raw, &eid).await;
    fcall_create_execution(raw, &eid).await;
    fcall_issue_claim_grant(raw, &eid).await;
    let handle = backend_claim(backend, &eid).await;

    // Suspend on a single waitpoint keyed by an "approved" matcher. The
    // approval flow under test delivers either "approved" or "rejected"
    // — the matcher is deliberately `Wildcard` in one test and by-name
    // in others; here we default to Wildcard so both signal names land.
    let wp_id = WaitpointId::new();
    let wp_key = format!("wpk:approval:{wp_id}");
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: wp_key.clone(),
    };
    let cond = ResumeCondition::Single {
        waitpoint_key: wp_key,
        matcher: SignalMatcher::Wildcard,
    };
    let suspend_args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let outcome = backend
        .suspend(&handle, suspend_args)
        .await
        .expect("suspend");
    let SuspendOutcome::Suspended { handle: sus_handle, .. } = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };

    (eid, sus_handle, wp_id)
}

fn approval_args(
    eid: &ExecutionId,
    wp_id: &WaitpointId,
    signal_name: &str,
    idempotency_suffix: &str,
) -> DeliverApprovalSignalArgs {
    DeliverApprovalSignalArgs::new(
        eid.clone(),
        LaneId::new(LANE),
        wp_id.clone(),
        signal_name.to_owned(),
        idempotency_suffix.to_owned(),
        60_000,
        None,
        None,
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn deliver_approval_signal_approved_resumes_execution() {
    let raw = raw_client().await;
    let backend = connect_backend().await;
    let (eid, _handle, wp_id) = setup_suspended(&backend, &raw).await;

    let result = backend
        .deliver_approval_signal(approval_args(&eid, &wp_id, "approved", "d1"))
        .await
        .expect("deliver_approval_signal approved");

    match result {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(
                effect, "resume_condition_satisfied",
                "approved signal on a single-wildcard waitpoint must resume"
            );
        }
        other => panic!("expected Accepted, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn deliver_approval_signal_rejected_resumes_execution() {
    let raw = raw_client().await;
    let backend = connect_backend().await;
    let (eid, _handle, wp_id) = setup_suspended(&backend, &raw).await;

    let result = backend
        .deliver_approval_signal(approval_args(&eid, &wp_id, "rejected", "d1"))
        .await
        .expect("deliver_approval_signal rejected");

    match result {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("expected Accepted, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn deliver_approval_signal_missing_waitpoint_is_not_found() {
    // Bootstrap an execution but NEVER suspend — the waitpoint row is
    // absent, so `read_waitpoint_token` returns Ok(None) and the impl
    // surfaces EngineError::NotFound { entity: "waitpoint" }. This is
    // the branch that proves the server-side token lookup exists: a
    // caller-supplied-token variant would 500 on HMAC instead.
    let raw = raw_client().await;
    ensure_default_partition_config(&raw).await;
    let backend = connect_backend().await;
    ensure_hmac_secret(&backend).await;

    let eid = new_eid();
    cleanup_execution(&raw, &eid).await;
    fcall_create_execution(&raw, &eid).await;
    fcall_issue_claim_grant(&raw, &eid).await;
    let _handle = backend_claim(&backend, &eid).await;

    let wp_id = WaitpointId::new();
    let err = backend
        .deliver_approval_signal(approval_args(&eid, &wp_id, "approved", "d1"))
        .await
        .expect_err("missing waitpoint must be NotFound");
    match err {
        EngineError::NotFound { entity } => assert_eq!(entity, "waitpoint"),
        other => panic!("expected NotFound {{ waitpoint }}, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn deliver_approval_signal_dedup_within_ttl_returns_duplicate() {
    // Same (signal_name, idempotency_suffix) inside the dedup window
    // must surface DeliverSignalResult::Duplicate rather than a second
    // delivery. The idempotency key is composed server-side from
    // "approval:<signal_name>:<idempotency_suffix>", so the caller
    // repeats via the same DeliverApprovalSignalArgs.
    //
    // The Lua `ff_deliver_signal` checks `waitpoint_closed` BEFORE the
    // idempotency branch (step 4 vs step 5 in flowfabric.lua), so the
    // dedup replay has to happen on a waitpoint that stays open after
    // the first delivery. Use a composite `Count{n=2,
    // DistinctSources}` condition: the first approval appends
    // (condition not yet satisfied — only one distinct source, and
    // operator approval signals all carry `source_identity=""`), which
    // keeps the waitpoint `active`; the second replay with the same
    // idempotency_suffix then hits the dedup branch and returns
    // Duplicate.
    let raw = raw_client().await;
    ensure_default_partition_config(&raw).await;
    let backend = connect_backend().await;
    ensure_hmac_secret(&backend).await;

    let eid = new_eid();
    cleanup_execution(&raw, &eid).await;
    fcall_create_execution(&raw, &eid).await;
    fcall_issue_claim_grant(&raw, &eid).await;
    let handle = backend_claim(&backend, &eid).await;

    let wp_id = WaitpointId::new();
    let wp_key = format!("wpk:approval-dedup:{wp_id}");
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: wp_key.clone(),
    };
    let cond = ResumeCondition::Composite(CompositeBody::Count {
        n: 2,
        count_kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key],
    });
    let suspend_args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let outcome = backend.suspend(&handle, suspend_args).await.expect("suspend");
    let SuspendOutcome::Suspended { .. } = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };

    let args1 = approval_args(&eid, &wp_id, "approved", "dedup-1");
    let first = backend
        .deliver_approval_signal(args1)
        .await
        .expect("first approval");
    let first_sid = match first {
        DeliverSignalResult::Accepted {
            signal_id, effect, ..
        } => {
            assert_eq!(
                effect, "appended_to_waitpoint",
                "count-2 distinct-sources must not satisfy on the first delivery"
            );
            signal_id
        }
        other => panic!("expected Accepted on first, got {other:?}"),
    };

    // Replay same args within TTL — Lua dedup fires on the idempotency
    // key and returns Duplicate carrying the original signal id.
    let args2 = approval_args(&eid, &wp_id, "approved", "dedup-1");
    let second = backend
        .deliver_approval_signal(args2)
        .await
        .expect("second approval");
    match second {
        DeliverSignalResult::Duplicate { existing_signal_id } => {
            assert_eq!(
                existing_signal_id, first_sid,
                "dedup should surface the first signal's id"
            );
        }
        other => panic!("expected Duplicate on replay, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}
