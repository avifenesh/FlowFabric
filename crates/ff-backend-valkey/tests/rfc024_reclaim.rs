//! RFC-024 PR-F — `issue_reclaim_grant` + `reclaim_execution` Valkey
//! integration tests.
//!
//! Gated `#[ignore]` because they require a live Valkey at
//! `127.0.0.1:6379` (same deployment as the sibling
//! `subscribe_lease_history` / `capabilities` tests). Run with:
//!
//! ```
//! valkey-cli -h 127.0.0.1 -p 6379 ping
//! cargo test -p ff-backend-valkey --test rfc024_reclaim -- --ignored
//! ```
//!
//! Shape: each test sets up a minimal lease-expired-reclaimable state
//! via raw FCALLs (create → issue_claim_grant → claim → mark_expired),
//! then drives the new `EngineBackend::issue_reclaim_grant` +
//! `reclaim_execution` trait methods and asserts the typed outcome
//! variants.
//!
//! The setup-FCALL helpers mirror `ff-test::e2e_lifecycle` but live
//! here so the valkey-backend crate's test surface doesn't take a
//! cyclic dep on `ff-test`.

#![allow(clippy::too_many_arguments)]

use std::collections::BTreeSet;
use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{BackendConfig, HandleKind};
use ff_core::contracts::{
    IssueReclaimGrantArgs, IssueReclaimGrantOutcome, ReclaimExecutionArgs,
    ReclaimExecutionOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, execution_partition};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, FlowId, LaneId, LeaseId, TimestampMs, WorkerId,
    WorkerInstanceId,
};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 6379;
const LANE: &str = "default";
const WORKER: &str = "rfc024-worker";
const WORKER_INST: &str = "rfc024-worker-inst";
const NS: &str = "rfc024-reclaim-ns";

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
        "rfc024-test".to_owned(),
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

async fn fcall_claim_execution(
    client: &ferriskey::Client,
    eid: &ExecutionId,
    lease_ttl_ms: u64,
) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);
    let att_idx = AttemptIndex::new(0);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.attempt_hash(att_idx),
        ctx.attempt_usage(att_idx),
        ctx.attempt_policy(att_idx),
        ctx.attempts(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];
    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    // ARGV (12)
    let args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        lease_id,
        lease_ttl_ms.to_string(),
        (lease_ttl_ms * 2 / 3).to_string(),
        attempt_id,
        "{}".to_owned(),
        String::new(),
        String::new(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = client
        .fcall("ff_claim_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_claim_execution");
}

async fn fcall_mark_lease_expired(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    // KEYS (4): exec_core, lease_current, lease_expiry, lease_history
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.lease_current(),
        idx.lease_expiry(),
        ctx.lease_history(),
    ];
    let args: Vec<String> = vec![eid.to_string()];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = client
        .fcall("ff_mark_lease_expired_if_due", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_mark_lease_expired_if_due");
}

async fn cleanup_execution(client: &ferriskey::Client, eid: &ExecutionId) {
    let partition = partition_for(eid);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    // Per-execution keys.
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
        ctx.attempt_hash(AttemptIndex::new(1)),
        ctx.attempt_usage(AttemptIndex::new(0)),
        ctx.attempt_usage(AttemptIndex::new(1)),
        ctx.attempt_policy(AttemptIndex::new(0)),
        ctx.attempt_policy(AttemptIndex::new(1)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(1)),
    ] {
        let _: Value = client.cmd("DEL").arg(k).execute().await.unwrap_or(Value::Nil);
    }

    // Index entries referencing this execution. ZREM / SREM are safe
    // no-ops if the member is absent; indices themselves are shared
    // across executions so we must not DEL them outright.
    for (cmd, key) in [
        ("ZREM", idx.lease_expiry()),
        ("ZREM", idx.lane_active(&lane_id)),
        ("ZREM", idx.lane_eligible(&lane_id)),
        ("ZREM", idx.lane_terminal(&lane_id)),
        ("ZREM", idx.attempt_timeout()),
        ("ZREM", idx.execution_deadline()),
        ("ZREM", idx.all_executions()),
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

/// Write `ff:config:partitions` matching `PartitionConfig::default()` so
/// the backend's `load_partition_config()` sees the same counts the test
/// uses when deriving keys. Without this, a pre-existing non-default
/// config in the shared Valkey would silently skew partition routing
/// and the test would assert against the wrong keys.
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

async fn setup_lease_expired_reclaimable(eid: &ExecutionId) -> ferriskey::Client {
    let client = raw_client().await;
    ensure_default_partition_config(&client).await;
    cleanup_execution(&client, eid).await;
    fcall_create_execution(&client, eid).await;
    fcall_issue_claim_grant(&client, eid).await;
    fcall_claim_execution(&client, eid, 100).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    fcall_mark_lease_expired(&client, eid).await;
    client
}

fn issue_args(eid: ExecutionId) -> IssueReclaimGrantArgs {
    IssueReclaimGrantArgs::new(
        eid,
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        LaneId::new(LANE),
        None,
        5_000,
        None,
        None,
        BTreeSet::new(),
        TimestampMs::now(),
    )
}

fn reclaim_args(eid: ExecutionId, max: Option<u32>) -> ReclaimExecutionArgs {
    ReclaimExecutionArgs::new(
        eid,
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        LaneId::new(LANE),
        None,
        LeaseId::new(),
        30_000,
        AttemptId::new(),
        "{}".to_owned(),
        max,
        WorkerInstanceId::new(WORKER_INST),
        AttemptIndex::new(0),
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn issue_reclaim_grant_on_expired_lease_returns_granted() {
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;

    let backend = connect_backend().await;
    let out = backend
        .issue_reclaim_grant(issue_args(eid.clone()))
        .await
        .expect("issue_reclaim_grant");

    match out {
        IssueReclaimGrantOutcome::Granted(grant) => {
            assert_eq!(grant.execution_id, eid);
            assert_eq!(grant.lane_id, LaneId::new(LANE));
            assert!(grant.expires_at_ms > 0);
        }
        other => panic!("expected Granted, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn issue_reclaim_grant_on_nonexistent_is_error() {
    // An execution that was never created → `execution_not_found` err
    // code → EngineError (not a typed outcome).
    let eid = new_eid();
    ensure_default_partition_config(&raw_client().await).await;
    let backend = connect_backend().await;
    let result = backend.issue_reclaim_grant(issue_args(eid)).await;
    assert!(result.is_err(), "expected EngineError on missing exec, got {result:?}");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reclaim_execution_happy_path_mints_reclaimed_handle() {
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;

    let backend = connect_backend().await;
    let grant_out = backend
        .issue_reclaim_grant(issue_args(eid.clone()))
        .await
        .expect("issue_reclaim_grant");
    assert!(matches!(grant_out, IssueReclaimGrantOutcome::Granted(_)));

    let out = backend
        .reclaim_execution(reclaim_args(eid.clone(), None))
        .await
        .expect("reclaim_execution");

    match out {
        ReclaimExecutionOutcome::Claimed(handle) => {
            assert_eq!(handle.kind, HandleKind::Reclaimed);
        }
        other => panic!("expected Claimed(Handle{{Reclaimed}}), got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reclaim_execution_without_grant_returns_grant_not_found() {
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;
    // Deliberately skip issue_reclaim_grant.

    let backend = connect_backend().await;
    let out = backend
        .reclaim_execution(reclaim_args(eid.clone(), None))
        .await
        .expect("reclaim_execution");

    match out {
        ReclaimExecutionOutcome::GrantNotFound { execution_id } => {
            assert_eq!(execution_id, eid);
        }
        other => panic!("expected GrantNotFound, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn issue_reclaim_grant_surfaces_server_authoritative_expires_at() {
    // RFC-024 §3.1 regression: the returned `ReclaimGrant.expires_at_ms`
    // must come from the server's `TIME` reading + `grant_ttl_ms` (via
    // the Lua's return payload), not the caller's Rust-side `now`. We
    // assert the returned timestamp matches the `grant_expires_at`
    // stored in the grant hash by the Lua itself — if the client-side
    // value was used, the two would drift under clock skew.
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;

    let backend = connect_backend().await;
    let grant_ttl_ms: u64 = 5_000;
    let args = IssueReclaimGrantArgs::new(
        eid.clone(),
        WorkerId::new(WORKER),
        WorkerInstanceId::new(WORKER_INST),
        LaneId::new(LANE),
        None,
        grant_ttl_ms,
        None,
        None,
        BTreeSet::new(),
        TimestampMs::now(),
    );
    let out = backend.issue_reclaim_grant(args).await.expect("issue");

    let partition = partition_for(&eid);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let grant_key = ctx.claim_grant();
    let stored: String = raw
        .cmd("HGET")
        .arg(&grant_key)
        .arg("grant_expires_at")
        .execute()
        .await
        .ok()
        .and_then(|v: Value| match v {
            Value::BulkString(b) => String::from_utf8(b.to_vec()).ok(),
            Value::SimpleString(s) => Some(s),
            _ => None,
        })
        .expect("grant hash has grant_expires_at");
    let stored: u64 = stored.parse().expect("grant_expires_at numeric");

    match out {
        IssueReclaimGrantOutcome::Granted(grant) => {
            assert_eq!(
                grant.expires_at_ms, stored,
                "expires_at_ms must come from server-side TIME, not client-side now"
            );
        }
        other => panic!("expected Granted, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reclaim_execution_cap_exceeded_at_argv9_exact_boundary() {
    // Investigation: per memory project_reclaim_cap_exceeded_investigation.md,
    // the PR-F (#402) agent observed `lease_reclaim_count=4, max=Some(4)`
    // returning `Claimed(Handle)` instead of `ReclaimCapExceeded`. This
    // test exercises the exact boundary with no policy override so the
    // code path is purely ARGV[9] + Lua `>=` comparison at
    // `flowfabric.lua:3065`.
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;

    let partition = partition_for(&eid);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let core_key = ctx.core();

    let _: Value = raw
        .cmd("HSET")
        .arg(&core_key)
        .arg("lease_reclaim_count")
        .arg("4")
        .execute()
        .await
        .expect("HSET lease_reclaim_count");

    let backend = connect_backend().await;
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone()))
        .await
        .expect("issue_reclaim_grant");

    let out = backend
        .reclaim_execution(reclaim_args(eid.clone(), Some(4)))
        .await
        .expect("reclaim_execution");

    match out {
        ReclaimExecutionOutcome::ReclaimCapExceeded {
            execution_id,
            reclaim_count,
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(
                reclaim_count, 4,
                "authoritative cap should surface ARGV[9] max=4"
            );
        }
        other => panic!(
            "expected ReclaimCapExceeded at lease_reclaim_count=max=4 boundary, got {other:?}"
        ),
    }

    cleanup_execution(&raw, &eid).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Valkey at 127.0.0.1:6379"]
async fn reclaim_execution_cap_exceeded_reports_policy_override_not_argv_default() {
    // RFC-024 §4.6 regression: when a per-execution policy override at
    // <core>:policy sets `max_reclaim_count`, the Lua enforces that cap
    // (not ARGV[9]). The `ReclaimCapExceeded.reclaim_count` must
    // surface the authoritative enforced value — prior bug echoed the
    // Rust-side ARGV[9] default, which diverges under override.
    let eid = new_eid();
    let raw = setup_lease_expired_reclaimable(&eid).await;

    // Set lease_reclaim_count above the policy cap so reclaim is
    // immediately terminal on the first call.
    let partition = partition_for(&eid);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let core_key = ctx.core();
    let policy_key = ctx.policy();

    let _: Value = raw
        .cmd("HSET")
        .arg(&core_key)
        .arg("lease_reclaim_count")
        .arg("5")
        .execute()
        .await
        .expect("HSET lease_reclaim_count");
    // Policy override: cap=3 (below the reclaim count of 5).
    let _: Value = raw
        .cmd("SET")
        .arg(&policy_key)
        .arg(r#"{"max_reclaim_count":3}"#)
        .execute()
        .await
        .expect("SET policy");

    let backend = connect_backend().await;
    // Caller-side ARGV[9] default is 1000 (Some(1000)); policy cap is 3.
    // If the old bug were present, `reclaim_count` would be 1000.
    let out = backend
        .reclaim_execution(reclaim_args(eid.clone(), Some(1000)))
        .await
        .expect("reclaim_execution");

    match out {
        ReclaimExecutionOutcome::ReclaimCapExceeded {
            execution_id,
            reclaim_count,
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(
                reclaim_count, 3,
                "must surface policy override cap (3), not ARGV[9] default (1000)"
            );
        }
        other => panic!("expected ReclaimCapExceeded, got {other:?}"),
    }

    cleanup_execution(&raw, &eid).await;
}
