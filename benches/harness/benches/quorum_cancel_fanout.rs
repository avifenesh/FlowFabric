//! Scenario 6 — RFC-016 Stage C quorum-cancel fan-out latency.
//!
//! Measures the wall-time from the moment an AnyOf/Quorum edge group
//! flips to `satisfied` under `OnSatisfied::CancelRemaining` to the
//! moment the last still-running sibling reaches `terminal_outcome =
//! cancelled`. This is the user-visible "kill the losers" latency.
//!
//! Shape (RFC-016 §4.2):
//!   - Fan-in N >= 10, Quorum(k=1, n=N), OnSatisfied::CancelRemaining
//!   - Seed N peer executions blocked on one downstream edge group
//!   - Resolve edge 0 as success -> group satisfied -> siblings enqueued
//!   - Poll the N-1 sibling exec_cores until all report
//!     `terminal_outcome = cancelled`
//!   - Record the p50 / p95 / p99 of that wall-time window across samples
//!
//! Points: n in {10, 100, 1000}.
//!
//! Ship SLO (§4.2): p99 end-to-end dispatch latency MUST stay within
//! 500 ms at n = 100; n = 10 and n = 1000 are characterization-only
//! (no pass/fail threshold in v0.6). This harness does NOT enforce
//! the SLO -- enforcement runs at the release gate via the
//! `benches/scripts/check_release.py` comparison. Here we only emit
//! the JSON report so the release gate has data.
//!
//! Invoked via:
//!   cargo bench -p ff-bench --bench quorum_cancel_fanout
//!
//! Prerequisites:
//!   - Valkey on FF_HOST:FF_PORT (default localhost:6379) with the
//!     FlowFabric library loadable.
//!   - No ff-server required — the harness spins up an in-process
//!     `ff_server::server::Server` with a tightened 250ms dispatcher
//!     interval so the SLO measurement is not dominated by scheduler
//!     cadence.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use ff_bench::{
    report::{LatencyMs, Percentiles},
    write_report, Report, SYSTEM_FLOWFABRIC,
};
use ff_core::contracts::{
    ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use ferriskey::Value;

const SCENARIO: &str = "quorum_cancel_fanout";
const NS_PREFIX: &str = "bench-qcf";
const LANE: &str = "bench-qcf-lane";

fn scenario_6(_c: &mut Criterion) {
    // NOTE: we deliberately don't use criterion's sampling here. Each
    // iter seeds O(n) executions + stages O(n) edges on a real Valkey,
    // and at n=1000 criterion's adaptive iteration-count would try
    // hundreds of iters per sample -- blowing the budget. Instead,
    // hand-roll exactly 10 samples per size.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let env = ff_bench::workload::BenchEnv::from_env();

    let samples_per_n: usize = std::env::var("FF_BENCH_QCF_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    // Points to characterize. n=100 is the SLO gate (§4.2); 10 and 1000
    // are characterization-only.
    let sizes: Vec<usize> = vec![10, 100, 1000];

    let mut size_samples: Vec<(usize, Vec<u64>)> = Vec::new();
    for &n in &sizes {
        eprintln!("quorum_cancel_fanout: n={n}, collecting {samples_per_n} samples");
        let mut samples_us: Vec<u64> = Vec::with_capacity(samples_per_n);
        for i in 0..samples_per_n {
            let d = rt.block_on(drain_once(n));
            eprintln!("  sample {}/{samples_per_n}: {:.3} ms", i + 1, d.as_secs_f64() * 1000.0);
            samples_us.push(d.as_micros() as u64);
        }
        size_samples.push((n, samples_us));
    }

    // Emit one JSON report per size (scenario name includes n so
    // check_release.py can diff per-point).
    for (n, samples) in &size_samples {
        let latency = Percentiles::from_micros(samples);
        let count = samples.len() as f64;
        let throughput = if count > 0.0 && latency.p50 > 0.0 {
            1000.0 / latency.p50
        } else {
            0.0
        };
        let config = serde_json::json!({
            "n": n,
            "k": 1,
            "on_satisfied": "cancel_remaining",
            "rfc": "RFC-016 Stage C §4.2",
            "slo_p99_ms_at_n_100": 500,
            "dispatcher_interval_ms": 250,
        });
        let scenario = format!("{SCENARIO}_n{n}");
        let report = Report::fill_env(
            scenario.clone(),
            SYSTEM_FLOWFABRIC,
            env.cluster,
            config,
            throughput,
            latency,
        );
        let results_dir = PathBuf::from("benches/results");
        match write_report(&report, &results_dir) {
            Ok(path) => eprintln!("wrote {}", path.display()),
            Err(e) => eprintln!("warn: failed to write bench report: {e}"),
        }
    }

    let _ = LatencyMs::default();
}

/// One drain measurement. Returns wall-time Duration from "group flips
/// to satisfied" (fcall ff_resolve_dependency success) through "last
/// sibling reaches terminal_outcome = cancelled".
async fn drain_once(n: usize) -> Duration {
    // Fresh TestCluster per iter -- cleanup ensures each run starts
    // from zero state on the shared Valkey instance. Serial across
    // iters because TestCluster::cleanup FLUSHALLs ff:* keys.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let server = start_server(&tc).await;

    // Unique namespace per-iter to avoid stream-key collisions across
    // samples (cleanup handles it, but extra insurance).
    let ns = Namespace::new(format!("{NS_PREFIX}-{}", uuid::Uuid::new_v4()));

    // Mint flow + n upstream siblings + 1 downstream.
    let fid = FlowId::new();
    let fp = flow_partition(&fid, tc.partition_config());
    let members: Vec<ExecutionId> = (0..(n + 1))
        .map(|_| tc.new_execution_id_on_partition(fp.index))
        .collect();
    let downstream = members[n].clone();
    let upstreams: Vec<ExecutionId> = members[..n].to_vec();

    // Create flow + members.
    server
        .create_flow(&CreateFlowArgs {
            flow_id: fid.clone(),
            flow_kind: "bench-qcf".into(),
            namespace: ns.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");

    for m in &members {
        let partition_id = execution_partition(m, tc.partition_config()).index;
        server
            .create_execution(&CreateExecutionArgs {
                execution_id: m.clone(),
                namespace: ns.clone(),
                lane_id: LaneId::new(LANE),
                execution_kind: "bench-qcf".into(),
                input_payload: b"{}".to_vec(),
                payload_encoding: Some("json".into()),
                priority: 0,
                creator_identity: "ff-bench".into(),
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
        server
            .add_execution_to_flow(&ff_core::contracts::AddExecutionToFlowArgs {
                flow_id: fid.clone(),
                execution_id: m.clone(),
                now: TimestampMs::now(),
            })
            .await
            .expect("add_execution_to_flow");
    }

    // Set AnyOf{CancelRemaining} = Quorum(1, n){CancelRemaining} on the
    // downstream group. Done via direct backend FCALL to keep the bench
    // hot-path free of SDK worker overhead.
    set_edge_group_policy(
        &tc,
        &fid,
        &downstream,
        EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
    )
    .await;

    // Stage + apply N edges. Read current graph_revision first; a
    // freshly-created flow may have already bumped the counter from 0.
    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(
            FlowKeyContext::new(&flow_partition(&fid, tc.partition_config()), &fid)
                .core()
                .as_str(),
        )
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    for u in &upstreams {
        rev = stage_and_apply(&server, &fid, u, &downstream, rev).await;
    }

    // ---- Hot measurement window ----
    // Fire upstream[0] as success. The dispatcher should observe the
    // pending_cancel_groups SET entry and cancel the other N-1 siblings
    // within its 250ms tick + Lua round-trip.
    let start = Instant::now();
    fcall_resolve_success(&tc, &fid, &downstream, &upstreams[0]).await;

    // Poll until the LAST sibling (upstreams[n-1]) reaches cancelled.
    // We poll the highest-indexed sibling because the dispatcher's
    // cancel_siblings_pending list is drained in insertion order; the
    // last one is a reasonable worst-case proxy for "all done."
    //
    // Generous deadline so we don't fail the iter on a stall at n=1000
    // (characterization data still useful). SLO enforcement is offline.
    let deadline = Duration::from_secs(30);
    loop {
        if sibling_cancelled(&tc, &upstreams[n - 1]).await {
            break;
        }
        if start.elapsed() > deadline {
            eprintln!(
                "warn: n={n} iter exceeded {:?} without all siblings cancelling",
                deadline
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let elapsed = start.elapsed();

    // Drop server (joins background tasks). Cleanup via next iter's
    // `tc.cleanup().await`.
    drop(server);

    elapsed
}

async fn start_server(tc: &TestCluster) -> std::sync::Arc<ff_server::server::Server> {
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let pc = *tc.partition_config();
    let config = ff_server::config::ServerConfig {        valkey: ff_server::config::ValkeyServerConfig { host: host, port: port, tls: ff_test::fixtures::env_flag("FF_TLS"), cluster: ff_test::fixtures::env_flag("FF_CLUSTER"), skip_library_load: false },





        partition_config: pc,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new(LANE)],
            edge_cancel_dispatcher_interval: Duration::from_millis(250),
            ..Default::default()
        },

        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    
};
    let server = ff_server::server::Server::start(config)
        .await
        .expect("Server::start");
    std::sync::Arc::new(server)
}

async fn seed_partition_config(tc: &TestCluster) {
    let c = *tc.partition_config();
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

/// Set edge-group policy via a direct HSET on the edgegroup hash.
/// Mirrors `worker.set_edge_group_policy` without the SDK round trip.
async fn set_edge_group_policy(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    policy: EdgeDependencyPolicy,
) {
    let partition = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, fid);
    let key = fctx.edgegroup(downstream);
    let on_sat_str = |os: OnSatisfied| -> String {
        match os {
            OnSatisfied::CancelRemaining => "cancel_remaining".to_string(),
            OnSatisfied::LetRun => "let_run".to_string(),
            _ => "cancel_remaining".to_string(),
        }
    };
    let (variant, k, on_sat) = match policy {
        EdgeDependencyPolicy::AllOf => ("all_of".to_string(), 0u32, String::new()),
        EdgeDependencyPolicy::AnyOf { on_satisfied } => {
            ("any_of".to_string(), 1, on_sat_str(on_satisfied))
        }
        EdgeDependencyPolicy::Quorum { k, on_satisfied } => {
            ("quorum".to_string(), k, on_sat_str(on_satisfied))
        }
        _ => ("all_of".to_string(), 0, String::new()),
    };
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg(key.as_str())
        .arg("policy_variant")
        .arg(variant.as_str())
        .arg("k")
        .arg(k.to_string().as_str())
        .arg("on_satisfied")
        .arg(on_sat.as_str())
        .execute()
        .await
        .unwrap();
}

async fn fcall_resolve_success(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    upstream: &ExecutionId,
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
        ff_core::keys::FlowIndexKeys::new(&partition).pending_cancel_groups();

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
        "success".into(),
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

async fn sibling_cancelled(tc: &TestCluster, eid: &ExecutionId) -> bool {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let v: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg("terminal_outcome")
        .execute()
        .await
        .unwrap();
    v.as_deref() == Some("cancelled")
}

criterion_group!(benches, scenario_6);
criterion_main!(benches);
