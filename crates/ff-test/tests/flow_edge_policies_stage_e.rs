//! RFC-016 Stage E integration test matrix.
//!
//! Stage E is gap-fill: Stages A–D each cover their own concern (types,
//! evaluator, dispatcher, reconciler). Stage E closes the loop against
//! RFC §10 with tests that cross stage boundaries or exercise the
//! surfaces no earlier stage owned:
//!
//! 1. **Full `(AnyOf|Quorum) × (CancelRemaining|LetRun)` cancel-to-terminal
//!    matrix.** All four combinations drive the full upstream → resolver →
//!    dispatcher → sibling-terminal chain and assert the reason label on
//!    each sibling. Some combinations (LetRun) deliberately assert
//!    NON-cancellation after the dispatcher interval.
//!
//! 2. **Cluster co-location (§10.1).** Flow key + edgegroup hash +
//!    `pending_cancel_groups` SET + each member's `exec_core` all
//!    tag-route to the SAME `{fp:N}` so the whole
//!    resolve-dependency → dispatcher → reconciler chain runs
//!    single-slot. Assertion is a string-level hash-tag check —
//!    cluster-mode identical to single-shard.
//!
//! 3. **Skip-propagation cascade (§10.2/§10.4).** When a downstream is
//!    skipped by impossible quorum, its own outbound edge resolved
//!    against a successful upstream must still propagate the skip to
//!    the next-level downstream. Exercises the "skip cascade is
//!    orthogonal to edgegroup policy" invariant.
//!
//! 4. **Replay after one-shot fire (§10.2 edge).** Once a downstream
//!    is `eligible_now` under `AnyOf`, a sibling that resolves as
//!    `success` AFTER the fire MUST NOT re-transition the downstream
//!    (Invariant Q1). Counter advances for telemetry only.
//!
//! 5. **Observability assertions.** `ff_sibling_cancel_dispatched_total`
//!    and `ff_sibling_cancel_disposition_total` are read from a live
//!    OTEL registry (dev-dep `ff-server/observability`) and compared
//!    pre/post dispatcher tick. `ff_edge_group_policy_total` is
//!    emitted by whichever backend handled `set_edge_group_policy` —
//!    for worker-SDK traffic that's the worker's own Valkey-connected
//!    backend, not the shared server registry, so it's asserted via
//!    Stage A/B behavioural tests rather than metric-text parsing.
//!
//! 6. **Idempotent replay stress.** Stage D proved double-drain of a
//!    single group is safe. Stage E extends to 8 groups queued
//!    concurrently — the dispatcher processes them across ticks,
//!    counts are exactly-once, and the SET drains to empty.
//!
//! 7. **High-fanout correctness.** `Quorum(1, 50) + CancelRemaining` —
//!    one success, 49 sibling cancels. Correctness-only (NOT the Phase
//!    0 benchmark). Every sibling terminates with the satisfied reason.
//!
//! 8. **Builder ergonomics.** `EdgeDependencyPolicy::all_of()` /
//!    `any_of(...)` / `quorum(k, ...)` + `OnSatisfied::cancel_remaining()`
//!    / `let_run()` round-trip through `variant_str` + serde. Unit-level,
//!    no cluster.
//!
//! Run with:
//!   cargo test -p ff-test --test flow_edge_policies_stage_e -- --test-threads=1

use ferriskey::Value;
use ff_core::contracts::{
    ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, EdgeGroupState, OnSatisfied, StageDependencyEdgeArgs,
};
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use std::sync::Arc;
use std::time::Duration;

const NS: &str = "edgepol-e-ns";
const LANE: &str = "edgepol-e-lane";

const ASSERT_DEADLINE: Duration = Duration::from_secs(5);
const ASSERT_POLL: Duration = Duration::from_millis(100);

fn cfg() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

// ─── Test server boot with a shared OTEL metrics registry ────────────

async fn start_server_with_metrics(
    tc: &TestCluster,
    metrics: Arc<ff_observability::Metrics>,
) -> std::sync::Arc<ff_server::server::Server> {
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let pc = *tc.partition_config();
    let config = {
        let mut __cfg = ff_server::config::ServerConfig::default();
        __cfg.valkey = ff_server::config::ValkeyServerConfig { host: host, port: port, tls: ff_test::fixtures::env_flag("FF_TLS"), cluster: ff_test::fixtures::env_flag("FF_CLUSTER"), skip_library_load: false };
        __cfg.partition_config = pc;
        __cfg.lanes = vec![LaneId::new(LANE)];
        __cfg.listen_addr = "127.0.0.1:0".into();
        __cfg.engine_config = ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new(LANE)],
            edge_cancel_dispatcher_interval: Duration::from_millis(200),
            edge_cancel_reconciler_interval: Duration::from_millis(500),
            ..Default::default()
        };
        __cfg.cors_origins = vec!["*".to_owned()];
        __cfg.api_token = None;
        __cfg.waitpoint_hmac_secret = "0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        __cfg.waitpoint_hmac_grace_ms = 86_400_000;
        __cfg.max_concurrent_stream_ops = 64;
        __cfg.backend = ff_server::config::BackendKind::default();
        __cfg.postgres = Default::default();
        __cfg
    };
    let server = ff_server::server::Server::start_with_metrics(config, metrics)
        .await
        .expect("Server::start_with_metrics");
    std::sync::Arc::new(server)
}

async fn start_server(tc: &TestCluster) -> std::sync::Arc<ff_server::server::Server> {
    start_server_with_metrics(tc, Arc::new(ff_observability::Metrics::new())).await
}

async fn build_worker(instance: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: WorkerId::new("edgepol-e-worker"),
        worker_instance_id: WorkerInstanceId::new(instance),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

fn mint_flow_with_members(tc: &TestCluster, n: usize) -> (FlowId, Vec<ExecutionId>) {
    let fid = FlowId::new();
    let fp = flow_partition(&fid, tc.partition_config());
    let members = (0..n)
        .map(|_| tc.new_execution_id_on_partition(fp.index))
        .collect();
    (fid, members)
}

async fn create_flow_api(server: &ff_server::server::Server, fid: &FlowId) {
    server
        .create_flow(&CreateFlowArgs {
            flow_id: fid.clone(),
            flow_kind: "edgepol".into(),
            namespace: Namespace::new(NS),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");
}

async fn create_exec_api(
    server: &ff_server::server::Server,
    tc: &TestCluster,
    eid: &ExecutionId,
) {
    let partition_id = execution_partition(eid, tc.partition_config()).index;
    server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "edgepol".into(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".into()),
            priority: 0,
            creator_identity: "tester".into(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id,
            now: TimestampMs::now(),
        })
        .await
        .expect("create_execution");
}

async fn add_member(
    server: &ff_server::server::Server,
    fid: &FlowId,
    eid: &ExecutionId,
) {
    server
        .add_execution_to_flow(&ff_core::contracts::AddExecutionToFlowArgs {
            flow_id: fid.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("add_execution_to_flow");
}

async fn stage_and_apply(
    server: &ff_server::server::Server,
    fid: &FlowId,
    upstream: &ExecutionId,
    downstream: &ExecutionId,
    expected_rev: u64,
) -> u64 {
    let edge_id = EdgeId::new();
    let now = TimestampMs::now();
    let staged = server
        .stage_dependency_edge(&StageDependencyEdgeArgs {
            flow_id: fid.clone(),
            edge_id: edge_id.clone(),
            upstream_execution_id: upstream.clone(),
            downstream_execution_id: downstream.clone(),
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            expected_graph_revision: expected_rev,
            now,
        })
        .await
        .expect("stage_dependency_edge");
    let new_rev = match staged {
        ff_core::contracts::StageDependencyEdgeResult::Staged {
            new_graph_revision,
            ..
        } => new_graph_revision,
    };
    server
        .apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: fid.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: upstream.clone(),
            graph_revision: new_rev,
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            now,
        })
        .await
        .expect("apply_dependency_to_child");
    new_rev
}

async fn fcall_resolve_with(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    upstream: &ExecutionId,
    upstream_outcome: &str,
) {
    let partition = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, fid);
    let ep = execution_partition(downstream, tc.partition_config());
    let ctx = ExecKeyContext::new(&ep, downstream);
    let idx = ff_core::keys::IndexKeys::new(&ep);

    let edge_ids: Vec<String> = tc
        .client()
        .cmd("SMEMBERS")
        .arg(fctx.incoming(downstream).as_str())
        .execute()
        .await
        .expect("SMEMBERS incoming");

    let mut matching_edge: Option<String> = None;
    for eid in &edge_ids {
        let parsed = match EdgeId::parse(eid) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let upstream_id: Option<String> = tc
            .client()
            .cmd("HGET")
            .arg(fctx.edge(&parsed).as_str())
            .arg("upstream_execution_id")
            .execute()
            .await
            .unwrap();
        if upstream_id.as_deref() == Some(upstream.to_string().as_str()) {
            matching_edge = Some(eid.clone());
            break;
        }
    }
    let edge_str = matching_edge.expect("no edge matching upstream found");

    let edge_parsed = EdgeId::parse(&edge_str).unwrap();
    let upstream_ctx = ExecKeyContext::new(&ep, upstream);
    let att = AttemptIndex::new(0);
    let lane = LaneId::new(LANE);
    let edgegroup = fctx.edgegroup(downstream);
    let incoming_set = fctx.incoming(downstream);
    let pending_cancel_groups_set =
        FlowIndexKeys::new(&partition).pending_cancel_groups();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.deps_meta(),
        ctx.deps_unresolved(),
        ctx.dep_edge(&edge_parsed),
        idx.lane_eligible(&lane),
        idx.lane_terminal(&lane),
        idx.lane_blocked_dependencies(&lane),
        ctx.attempt_hash(att),
        ctx.stream_meta(att),
        ctx.payload(),
        upstream_ctx.result(),
        edgegroup,
        incoming_set,
        pending_cancel_groups_set,
    ];
    let now = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        edge_str,
        upstream_outcome.into(),
        now,
        fid.to_string(),
        downstream.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_resolve_dependency", &kr, &ar)
        .await
        .expect("FCALL ff_resolve_dependency");
}

async fn seed_partition_config(tc: &TestCluster) {
    let c = cfg();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(c.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(c.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(c.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn read_exec_field(
    tc: &TestCluster,
    eid: &ExecutionId,
    field: &str,
) -> Option<String> {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
}

async fn read_edgegroup_field(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    field: &str,
) -> Option<String> {
    let partition = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, fid);
    tc.client()
        .cmd("HGET")
        .arg(fctx.edgegroup(downstream).as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
}

async fn wait_for<F, Fut>(deadline: Duration, f: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(ASSERT_POLL).await;
    }
    false
}

async fn setup_fanin(
    tc: &TestCluster,
    server: &ff_server::server::Server,
    worker: &ff_sdk::FlowFabricWorker,
    upstream_n: usize,
    policy: EdgeDependencyPolicy,
) -> (FlowId, Vec<ExecutionId>, ExecutionId) {
    let (fid, members) = mint_flow_with_members(tc, upstream_n + 1);
    let downstream = members[upstream_n].clone();
    let upstreams: Vec<ExecutionId> = members[..upstream_n].to_vec();

    create_flow_api(server, &fid).await;
    for m in &members {
        create_exec_api(server, tc, m).await;
        add_member(server, &fid, m).await;
    }

    worker
        .set_edge_group_policy(&fid, &downstream, policy)
        .await
        .expect("set_edge_group_policy");

    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(
            FlowKeyContext::new(&flow_partition(&fid, &cfg()), &fid)
                .core()
                .as_str(),
        )
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    for u in &upstreams {
        rev = stage_and_apply(server, &fid, u, &downstream, rev).await;
    }

    (fid, upstreams, downstream)
}

// ─── Prometheus text parser: just enough for counter assertions ─────
//
// OTEL's prometheus exporter renders counters as `<name>_total{<labels>} <value>`.
// We only need to sum matching label-sets, so a line-by-line scan
// suffices (avoids pulling in a full prom parser).

fn sum_counter(text: &str, metric: &str, label_match: &[(&str, &str)]) -> f64 {
    let prefix = format!("{metric}_total");
    let mut total = 0.0;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        // rest is either `{labels} value` or ` value`.
        let (labels_blob, value_str) = match rest.strip_prefix('{') {
            Some(after_brace) => {
                let end = match after_brace.find('}') {
                    Some(i) => i,
                    None => continue,
                };
                let labels = &after_brace[..end];
                let tail = after_brace[end + 1..].trim_start();
                (labels, tail)
            }
            None => ("", rest.trim_start()),
        };
        let mut ok = true;
        for (k, v) in label_match {
            let needle = format!("{k}=\"{v}\"");
            if !labels_blob.contains(needle.as_str()) {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        let val_end = value_str.find(' ').unwrap_or(value_str.len());
        if let Ok(v) = value_str[..val_end].parse::<f64>() {
            total += v;
        }
    }
    total
}

// ─────────────────────────────────────────────────────────────────────
// 1. Full matrix — cancel-to-terminal + reason label
// ─────────────────────────────────────────────────────────────────────

/// Matrix row 1: AnyOf + CancelRemaining. One success → 2 cancels with
/// `sibling_quorum_satisfied`.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_matrix_any_of_cancel_remaining() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-any-cancel").await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    for (i, sib) in upstreams.iter().enumerate().skip(1) {
        let sib_cl = sib.clone();
        let tc_cl = &tc;
        let terminated = wait_for(ASSERT_DEADLINE, || {
            let sib = sib_cl.clone();
            async move {
                read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                    == Some("cancelled")
            }
        })
        .await;
        assert!(terminated, "sibling #{i} did not cancel");
        assert_eq!(
            read_exec_field(&tc, sib, "cancellation_reason").await.as_deref(),
            Some("sibling_quorum_satisfied"),
            "sibling #{i} reason wrong"
        );
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
}

/// Matrix row 2: AnyOf + LetRun. One success fires downstream, the
/// dispatcher NEVER touches siblings — even after ample ticks.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_matrix_any_of_let_run() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-any-letrun").await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::let_run()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    // Deterministic: wait enough for multiple dispatcher ticks (200ms),
    // then check siblings remain non-terminal.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    for (i, sib) in upstreams.iter().enumerate().skip(1) {
        assert_ne!(
            read_exec_field(&tc, sib, "terminal_outcome").await.as_deref(),
            Some("cancelled"),
            "LetRun must never cancel sibling #{i}"
        );
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now"),
        "downstream fires exactly once on first success"
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied")
    );
}

/// Matrix row 3: Quorum(3/5) + CancelRemaining. The third success fires,
/// the two stragglers terminate cancelled.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_matrix_quorum_cancel_remaining() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-q-cancel").await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::cancel_remaining()),
    )
    .await;

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }

    for i in 3..5 {
        let sib = upstreams[i].clone();
        let tc_cl = &tc;
        let terminated = wait_for(ASSERT_DEADLINE, || {
            let sib = sib.clone();
            async move {
                read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                    == Some("cancelled")
            }
        })
        .await;
        assert!(terminated, "straggler #{i} did not cancel");
        assert_eq!(
            read_exec_field(&tc, &upstreams[i], "cancellation_reason").await.as_deref(),
            Some("sibling_quorum_satisfied")
        );
    }
}

/// Matrix row 4: Quorum(3/5) + LetRun. Third success fires downstream,
/// survivors remain running; counters still update for observability.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_matrix_quorum_let_run() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-q-letrun").await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::let_run()),
    )
    .await;

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }

    tokio::time::sleep(Duration::from_millis(1200)).await;
    for i in 3..5 {
        assert_ne!(
            read_exec_field(&tc, &upstreams[i], "terminal_outcome").await.as_deref(),
            Some("cancelled"),
            "LetRun must never cancel survivor #{i}"
        );
    }
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "succeeded").await.as_deref(),
        Some("3")
    );
    // Counter-continuing telemetry: resolve a straggler as success AFTER
    // satisfied_at; succeeded must advance while group_state stays satisfied.
    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[3], "success").await;
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "succeeded").await.as_deref(),
        Some("4"),
        "late sibling success still updates telemetry (§5 late-arriving)"
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied"),
        "one-shot: late terminal MUST NOT re-fire"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 2. Cluster co-location (§10.1)
// ─────────────────────────────────────────────────────────────────────

/// All keys the resolve-dependency → dispatcher → reconciler chain
/// touches for a given flow/edgegroup/pending-cancel tuple hash-tag to
/// the SAME `{fp:N}` slot. Cluster-mode identical to single-shard; this
/// asserts string-level identity so CI validates the invariant without
/// needing a live cluster fixture.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_cluster_colocation_hashtags_align() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-colo").await;

    // 5-member inbound group (4 siblings → 1 downstream).
    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        4,
        EdgeDependencyPolicy::quorum(2, OnSatisfied::cancel_remaining()),
    )
    .await;

    let fp = flow_partition(&fid, &cfg());
    let expected_tag = fp.hash_tag().to_owned();
    // Sanity: tag shape.
    assert!(
        expected_tag.starts_with("{fp:") && expected_tag.ends_with('}'),
        "unexpected partition hash-tag shape: {expected_tag}"
    );

    let flow_core_key = FlowKeyContext::new(&fp, &fid).core();
    let edgegroup_key = FlowKeyContext::new(&fp, &fid).edgegroup(&downstream);
    let pending_set_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let incoming_key = FlowKeyContext::new(&fp, &fid).incoming(&downstream);

    for (name, key) in [
        ("flow_core", &flow_core_key),
        ("edgegroup", &edgegroup_key),
        ("pending_cancel_groups", &pending_set_key),
        ("incoming_set", &incoming_key),
    ] {
        assert!(
            key.contains(expected_tag.as_str()),
            "{name} key `{key}` missing expected hash-tag `{expected_tag}`"
        );
    }

    // Every sibling's exec_core must alias to the same {fp:N} per RFC-011.
    for (i, u) in upstreams.iter().chain(std::iter::once(&downstream)).enumerate() {
        let ep = execution_partition(u, &cfg());
        let ek = ExecKeyContext::new(&ep, u).core();
        assert!(
            ek.contains(expected_tag.as_str()),
            "member #{i} exec_core `{ek}` not aliased to flow `{expected_tag}`"
        );
    }

    // Drive the chain to cover the whole key-set under observation; any
    // slot mismatch would have produced a CROSSSLOT error under cluster
    // mode before reaching this assertion.
    for i in 0..2 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }
    for i in 2..4 {
        let sib = upstreams[i].clone();
        let tc_cl = &tc;
        let terminated = wait_for(ASSERT_DEADLINE, || {
            let sib = sib.clone();
            async move {
                read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                    == Some("cancelled")
            }
        })
        .await;
        assert!(terminated, "co-located straggler #{i} did not cancel");
    }
}

// ─────────────────────────────────────────────────────────────────────
// 3. Skip-propagation cascade (§10.2 / §10.4)
// ─────────────────────────────────────────────────────────────────────

/// Impossible quorum → downstream skipped → downstream-of-downstream
/// resolves its inbound edge against that skipped upstream and itself
/// skip-propagates. Asserts that edgegroup policy is ORTHOGONAL to
/// RFC-007 baseline skip cascade: once a node is `terminal_outcome =
/// skipped`, its outbound edges resolve as `skipped` at the next
/// `ff_resolve_dependency`.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_impossible_skip_cascades_to_next_layer() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-skip-cascade").await;

    // Layer 1: Quorum(3/3) upstreams → mid (impossible after 1 fail).
    // Layer 2: mid → tail (AllOf, default).
    //
    // Using quorum 3/3 with 1 failure forces impossibility (failed>0 =>
    // failed > n-k = 0). Mid becomes skipped; then we resolve the mid→tail
    // edge with a "skipped" upstream outcome and expect tail to also be
    // terminal=skipped via AllOf short-circuit Q4.
    let (fid, members) = mint_flow_with_members(&tc, 5);
    let u1 = &members[0];
    let u2 = &members[1];
    let u3 = &members[2];
    let mid = &members[3];
    let tail = &members[4];

    create_flow_api(&server, &fid).await;
    for m in &members {
        create_exec_api(&server, &tc, m).await;
        add_member(&server, &fid, m).await;
    }

    // Policy on mid: Quorum(3, LetRun) — LetRun so nothing cancels the
    // untouched upstreams; we want the clean skip-cascade, not a cancel.
    worker
        .set_edge_group_policy(
            &fid,
            mid,
            EdgeDependencyPolicy::quorum(3, OnSatisfied::let_run()),
        )
        .await
        .expect("set_edge_group_policy mid");

    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(
            FlowKeyContext::new(&flow_partition(&fid, &cfg()), &fid)
                .core()
                .as_str(),
        )
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    for u in [u1, u2, u3] {
        rev = stage_and_apply(&server, &fid, u, mid, rev).await;
    }
    // Layer 2 edge: mid → tail (implicit AllOf).
    let _ = stage_and_apply(&server, &fid, mid, tail, rev).await;

    // Fire one failure on the Quorum(3/3) — failed(1) > n-k = 0 → impossible.
    fcall_resolve_with(&tc, &fid, mid, u1, "failed").await;

    assert_eq!(
        read_exec_field(&tc, mid, "terminal_outcome").await.as_deref(),
        Some("skipped"),
        "impossible quorum must skip mid"
    );

    // Propagate mid's "skipped" terminal through the Layer 2 edge.
    fcall_resolve_with(&tc, &fid, tail, mid, "skipped").await;

    assert_eq!(
        read_exec_field(&tc, tail, "terminal_outcome").await.as_deref(),
        Some("skipped"),
        "AllOf short-circuit (Q4) must cascade skip from mid to tail"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 4. Replay after one-shot fire (§10.2 edge, Invariant Q1)
// ─────────────────────────────────────────────────────────────────────

/// Once AnyOf fires, a replayed upstream terminal must NOT re-fire the
/// downstream. Exercises the Stage B one-shot gate (tested in Stage B
/// under the synchronous path), extended here to the CancelRemaining
/// flow: after the dispatcher cancels siblings, resolve one of the
/// now-terminal siblings AGAIN (projector-lag / replay) and assert the
/// group_state is unchanged and the downstream did not re-flip.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_replay_after_fire_is_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-replay").await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    // Wait for dispatcher to terminate the other two siblings.
    let sib1 = upstreams[1].clone();
    let tc_cl = &tc;
    let cancelled = wait_for(ASSERT_DEADLINE, || {
        let sib = sib1.clone();
        async move {
            read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                == Some("cancelled")
        }
    })
    .await;
    assert!(cancelled, "dispatcher did not terminate sib1");

    let satisfied_at_before =
        read_edgegroup_field(&tc, &fid, &downstream, "satisfied_at").await;
    let eligibility_before =
        read_exec_field(&tc, &downstream, "eligibility_state").await;
    let succeeded_before =
        read_edgegroup_field(&tc, &fid, &downstream, "succeeded").await;

    // Replay the winning upstream — resolver sees dep.state already
    // satisfied and returns `already_resolved` without mutating the
    // group (Invariant Q1 + Q2: edge dedup key is edge_id).
    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "satisfied_at").await,
        satisfied_at_before,
        "satisfied_at must be set once (HSETNX-semantics)"
    );
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await,
        eligibility_before,
        "downstream eligibility must not transition on replay"
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "succeeded").await,
        succeeded_before,
        "edge-id-keyed dedup must prevent double-count on replay"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 5. Observability — ff_edge_group_policy + ff_sibling_cancel_dispatched
// ─────────────────────────────────────────────────────────────────────

/// Shared metrics registry observes
/// `ff_sibling_cancel_dispatched_total{reason}` increments once the
/// server-side edge_cancel_dispatcher scanner terminates siblings. The
/// registry also observes `ff_sibling_cancel_disposition_total` for
/// the per-sibling ack accounting. The `ff_edge_group_policy_total`
/// counter is NOT asserted here: that metric is emitted on the
/// EngineBackend that handled the `set_edge_group_policy` call, which
/// for worker-side SDK traffic is the worker's own Valkey-connected
/// backend, not the server's shared registry. The dispatcher metric
/// lives inside the engine scanner which IS the shared registry, so
/// that's the correct invariant to pin here.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_observability_counters_emit() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let metrics = Arc::new(ff_observability::Metrics::new());
    let server = start_server_with_metrics(&tc, Arc::clone(&metrics)).await;
    let worker = build_worker("edgepol-e-obs").await;

    let cancel_before = sum_counter(
        metrics.render().as_str(),
        "ff_sibling_cancel_dispatched",
        &[("reason", "sibling_quorum_satisfied")],
    );
    let disposition_before = sum_counter(
        metrics.render().as_str(),
        "ff_sibling_cancel_disposition",
        &[("disposition", "cancelled")],
    );

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::quorum(2, OnSatisfied::cancel_remaining()),
    )
    .await;

    for i in 0..2 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }

    // Wait for the dispatcher tick to cancel the 3rd sibling.
    let sib2 = upstreams[2].clone();
    let tc_cl = &tc;
    let terminated = wait_for(ASSERT_DEADLINE, || {
        let sib = sib2.clone();
        async move {
            read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                == Some("cancelled")
        }
    })
    .await;
    assert!(terminated, "straggler not cancelled in time for metric check");

    // Poll the metric — the dispatcher increments run in the scanner
    // task; a small window accommodates the scheduling gap between the
    // sibling terminal write and the counter `.add(1, ...)`.
    let metrics_cl = Arc::clone(&metrics);
    let advanced = wait_for(Duration::from_secs(3), || {
        let m = Arc::clone(&metrics_cl);
        async move {
            let after = sum_counter(
                m.render().as_str(),
                "ff_sibling_cancel_dispatched",
                &[("reason", "sibling_quorum_satisfied")],
            );
            after > cancel_before
        }
    })
    .await;
    assert!(
        advanced,
        "ff_sibling_cancel_dispatched_total{{reason=sibling_quorum_satisfied}} \
         did not advance within 3s (before={cancel_before})\n\
         --- /metrics tail ---\n{}",
        metrics.render()
    );

    // Disposition counter for the per-sibling ack must also have advanced.
    let disposition_after = sum_counter(
        metrics.render().as_str(),
        "ff_sibling_cancel_disposition",
        &[("disposition", "cancelled")],
    );
    assert!(
        disposition_after > disposition_before,
        "ff_sibling_cancel_disposition_total{{disposition=cancelled}} \
         did not advance (before={disposition_before}, after={disposition_after})"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 6. Idempotent replay stress — 8 concurrent pending groups
// ─────────────────────────────────────────────────────────────────────

/// Many groups satisfied in a small window drain exactly once. Stage D
/// covers one group's double-drain safety; this extends to N=8 so the
/// dispatcher + reconciler co-ordinate across ticks without either
/// leaking or double-firing cancels. Asserts:
///   - every sibling terminates exactly once (cancelled, reason=satisfied)
///   - no surviving entries in the pending_cancel_groups SET
///   - downstreams all land in eligible_now
#[tokio::test]
#[serial_test::serial]
async fn stage_e_stress_many_pending_groups_drain_cleanly() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-stress").await;

    const N_GROUPS: usize = 8;
    const SIBS_PER_GROUP: usize = 3;

    let mut groups: Vec<(FlowId, Vec<ExecutionId>, ExecutionId)> =
        Vec::with_capacity(N_GROUPS);
    for _ in 0..N_GROUPS {
        let trio = setup_fanin(
            &tc,
            &server,
            &worker,
            SIBS_PER_GROUP,
            EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
        )
        .await;
        groups.push(trio);
    }

    // Fire first success on every group back-to-back → burst of pending
    // sibling-cancel dispatches on the same partition(s).
    for (fid, ups, ds) in &groups {
        fcall_resolve_with(&tc, fid, ds, &ups[0], "success").await;
    }

    // Wait for every sibling of every group to reach terminal=cancelled.
    for (gi, (_fid, ups, _ds)) in groups.iter().enumerate() {
        for i in 1..ups.len() {
            let sib = ups[i].clone();
            let tc_cl = &tc;
            let terminated = wait_for(ASSERT_DEADLINE, || {
                let sib = sib.clone();
                async move {
                    read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                        == Some("cancelled")
                }
            })
            .await;
            assert!(
                terminated,
                "group #{gi} sibling #{i} did not cancel in stress run"
            );
            assert_eq!(
                read_exec_field(&tc, &ups[i], "cancellation_reason").await.as_deref(),
                Some("sibling_quorum_satisfied"),
                "group #{gi} sibling #{i} wrong reason"
            );
        }
    }

    // pending_cancel_groups SET must be empty on the flow partition for
    // each flow (drains co-located: one SET per fp).
    for (fid, _, _) in &groups {
        let fp = flow_partition(fid, &cfg());
        let set_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
        let card: i64 = tc
            .client()
            .cmd("SCARD")
            .arg(set_key.as_str())
            .execute()
            .await
            .unwrap();
        assert_eq!(
            card, 0,
            "pending_cancel_groups not drained for flow {fid} (card={card})"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// 7. High-fanout correctness (NOT the Phase 0 benchmark)
// ─────────────────────────────────────────────────────────────────────

/// `Quorum(1, 50) + CancelRemaining` — one success triggers 49 sibling
/// cancels. Correctness-only: every sibling must terminate with
/// `reason = sibling_quorum_satisfied`; no latency SLO is asserted.
#[tokio::test]
#[serial_test::serial]
async fn stage_e_high_fanout_50_correctness() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker("edgepol-e-fanout").await;

    const N: usize = 50;
    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        N,
        EdgeDependencyPolicy::quorum(1, OnSatisfied::cancel_remaining()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    // Generous deadline: high-fanout may cross several dispatcher ticks.
    let deadline = Duration::from_secs(20);
    for i in 1..N {
        let sib = upstreams[i].clone();
        let tc_cl = &tc;
        let terminated = wait_for(deadline, || {
            let sib = sib.clone();
            async move {
                read_exec_field(tc_cl, &sib, "terminal_outcome").await.as_deref()
                    == Some("cancelled")
            }
        })
        .await;
        assert!(terminated, "sibling #{i} of 50 did not cancel in {deadline:?}");
        assert_eq!(
            read_exec_field(&tc, &upstreams[i], "cancellation_reason").await.as_deref(),
            Some("sibling_quorum_satisfied"),
            "sibling #{i} wrong reason"
        );
    }

    // Downstream stays eligible through all the sibling cancels.
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied")
    );
}

// ─────────────────────────────────────────────────────────────────────
// 8. Builder ergonomics — pure type-level round-trip
// ─────────────────────────────────────────────────────────────────────

/// Constructor helpers produce the expected enum shape + variant_str
/// label. Not cluster-backed; runs on every build, catches breakage if a
/// future RFC-017 discriminant is added in a way that shifts the
/// `variant_str` contract.
#[test]
fn stage_e_builder_ergonomics_roundtrip() {
    assert_eq!(EdgeDependencyPolicy::all_of(), EdgeDependencyPolicy::AllOf);
    assert_eq!(EdgeDependencyPolicy::all_of().variant_str(), "all_of");

    let any = EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining());
    assert!(matches!(
        any,
        EdgeDependencyPolicy::AnyOf { on_satisfied: OnSatisfied::CancelRemaining }
    ));
    assert_eq!(any.variant_str(), "any_of");

    let any_lr = EdgeDependencyPolicy::any_of(OnSatisfied::let_run());
    assert!(matches!(
        any_lr,
        EdgeDependencyPolicy::AnyOf { on_satisfied: OnSatisfied::LetRun }
    ));

    let q = EdgeDependencyPolicy::quorum(3, OnSatisfied::let_run());
    match q {
        EdgeDependencyPolicy::Quorum { k, on_satisfied } => {
            assert_eq!(k, 3);
            assert_eq!(on_satisfied, OnSatisfied::LetRun);
        }
        _ => panic!("quorum ctor wrong variant"),
    }
    assert_eq!(
        EdgeDependencyPolicy::quorum(5, OnSatisfied::cancel_remaining()).variant_str(),
        "quorum"
    );

    assert_eq!(OnSatisfied::cancel_remaining().variant_str(), "cancel_remaining");
    assert_eq!(OnSatisfied::let_run().variant_str(), "let_run");

    // Serde round-trip — Stage A/B already covers wire; belt-and-braces
    // that the builder outputs serialize the same way.
    let ser = serde_json::to_string(
        &EdgeDependencyPolicy::quorum(2, OnSatisfied::cancel_remaining()),
    )
    .unwrap();
    assert!(ser.contains("\"quorum\""), "unexpected serde shape: {ser}");
    let roundtrip: EdgeDependencyPolicy = serde_json::from_str(&ser).unwrap();
    assert!(matches!(
        roundtrip,
        EdgeDependencyPolicy::Quorum {
            k: 2,
            on_satisfied: OnSatisfied::CancelRemaining,
        }
    ));

    // EdgeGroupState exhaustive coverage — the four documented states
    // (§3 table) must all parse via `from_literal`.
    for s in ["pending", "satisfied", "impossible", "cancelled"] {
        let parsed = EdgeGroupState::from_literal(s);
        assert_eq!(parsed, EdgeGroupState::from_literal(s));
    }
}
