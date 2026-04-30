//! UC-27/21 `submit` CLI for the deploy-approval pipeline.
//!
//! DAG:
//!   build -> [unit, integration, e2e] (AllOf) -> deploy -> verify
//!
//! AllOf is the default EdgeDependencyPolicy (RFC-007 Stage A) so no
//! explicit set_edge_group_policy call is needed; the edge reducer
//! gates `deploy` until all three test executions terminate
//! successfully. Tests claim via capability-routed lanes
//! (kind=unit|integration|e2e, RFC-009).

use std::collections::BTreeSet;
use std::time::Duration;

use clap::Parser;
use deploy_approval_example::{
    BuildPayload, DeployPayload, TestPayload, VerifyPayload, CAP_ROLE_BUILD, CAP_ROLE_DEPLOY,
    CAP_ROLE_VERIFY, CAP_TEST_E2E, CAP_TEST_INTEGRATION, CAP_TEST_UNIT, DEFAULT_NAMESPACE,
    DEFAULT_SERVER_URL, EXECUTION_KIND_BUILD, EXECUTION_KIND_DEPLOY, EXECUTION_KIND_TEST,
    EXECUTION_KIND_VERIFY, LANE_BUILD, LANE_DEPLOY, LANE_TEST, LANE_VERIFY,
};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::{
    EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use ff_sdk::{FlowFabricWorker, WorkerConfig};

#[derive(Parser)]
#[command(name = "submit", about = "Submit a deploy-approval pipeline flow")]
struct Args {
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,
    #[arg(long, default_value = "app:demo")]
    artifact: String,
    #[arg(long, default_value = "deadbeef")]
    commit: String,
    /// Storage backend for documentation purposes. ff-server 0.6.1 is
    /// Valkey-only; postgres is rejected until the backend is wired
    /// through server config (see README Known gaps).
    #[arg(long, default_value = "valkey")]
    backend: String,
    #[arg(long, default_value_t = 256)]
    flow_partitions: u16,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
    #[arg(long, env = "FF_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,
    /// When true, the verify execution reports failure and triggers
    /// cancel_flow CancelAll against its own flow_id.
    #[arg(long)]
    fail_verify: bool,
    #[arg(long, default_value_t = 2)]
    poll_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deploy_approval_example=info,submit=info".into()),
        )
        .init();

    let args = Args::parse();
    if args.backend != "valkey" {
        eprintln!(
            "backend={} not supported by ff-server 0.6.1 (Valkey-only). See README Known gaps. Proceeding with Valkey.",
            args.backend
        );
    }

    let http = reqwest::Client::new();
    let partition_config = PartitionConfig {
        num_flow_partitions: args.flow_partitions,
        ..PartitionConfig::default()
    };
    let flow_id = FlowId::new();
    let partition = flow_partition(&flow_id, &partition_config).index;

    let build_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let unit_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let integ_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let e2e_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let deploy_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let verify_eid = ExecutionId::for_flow(&flow_id, &partition_config);

    let now_ms = TimestampMs::now().0;
    http_post(
        &http,
        &args,
        &format!("{}/v1/flows", args.server),
        &serde_json::json!({
            "flow_id": flow_id,
            "flow_kind": "deploy_approval",
            "namespace": args.namespace,
            "now": now_ms,
        }),
    )
    .await?;
    println!("[timeline] flow_created flow_id={flow_id}");

    let build_payload = BuildPayload {
        artifact: args.artifact.clone(),
        commit_sha: args.commit.clone(),
    };
    create_exec(
        &http,
        &args,
        &build_eid,
        partition,
        EXECUTION_KIND_BUILD,
        LANE_BUILD,
        &serde_json::to_vec(&build_payload)?,
        &BTreeSet::from([CAP_ROLE_BUILD.to_owned()]),
    )
    .await?;
    add_member(&http, &args, &flow_id, &build_eid).await?;
    println!("[timeline] build_created execution_id={build_eid}");

    let approval_wp_key = format!("wpk:deploy-approval:{}", uuid::Uuid::new_v4());
    for (eid, kind, cap) in [
        (&unit_eid, "unit", CAP_TEST_UNIT),
        (&integ_eid, "integration", CAP_TEST_INTEGRATION),
        (&e2e_eid, "e2e", CAP_TEST_E2E),
    ] {
        let payload = TestPayload {
            kind: kind.to_owned(),
            artifact: args.artifact.clone(),
        };
        create_exec(
            &http,
            &args,
            eid,
            partition,
            EXECUTION_KIND_TEST,
            LANE_TEST,
            &serde_json::to_vec(&payload)?,
            &BTreeSet::from([cap.to_owned()]),
        )
        .await?;
        add_member(&http, &args, &flow_id, eid).await?;
        println!("[timeline] test_created kind={kind} execution_id={eid}");
    }

    let deploy_payload = DeployPayload {
        artifact: args.artifact.clone(),
        approval_waitpoint_key: approval_wp_key.clone(),
        canary_percent: 10,
    };
    create_exec(
        &http,
        &args,
        &deploy_eid,
        partition,
        EXECUTION_KIND_DEPLOY,
        LANE_DEPLOY,
        &serde_json::to_vec(&deploy_payload)?,
        &BTreeSet::from([CAP_ROLE_DEPLOY.to_owned()]),
    )
    .await?;
    add_member(&http, &args, &flow_id, &deploy_eid).await?;
    println!("[timeline] deploy_created execution_id={deploy_eid}");

    let verify_payload = VerifyPayload {
        artifact: args.artifact.clone(),
        flow_id: flow_id.to_string(),
        fail: args.fail_verify,
    };
    create_exec(
        &http,
        &args,
        &verify_eid,
        partition,
        EXECUTION_KIND_VERIFY,
        LANE_VERIFY,
        &serde_json::to_vec(&verify_payload)?,
        &BTreeSet::from([CAP_ROLE_VERIFY.to_owned()]),
    )
    .await?;
    add_member(&http, &args, &flow_id, &verify_eid).await?;
    println!("[timeline] verify_created execution_id={verify_eid}");

    // Default AllOf policy on all inbound edge groups (RFC-007). Seed
    // graph_revision from describe_flow (SDK, no REST route).
    let sdk = FlowFabricWorker::connect(WorkerConfig {
        backend: Some(ff_core::backend::BackendConfig::valkey(&args.host, args.port)),
        worker_id: WorkerId::new("deploy-submit"),
        worker_instance_id: WorkerInstanceId::new(format!(
            "deploy-submit-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: Namespace::new(&args.namespace),
        lanes: vec![LaneId::new(LANE_DEPLOY)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
    })
    .await?;
    let snap = sdk
        .describe_flow(&flow_id)
        .await?
        .ok_or("describe_flow returned None right after create_flow")?;
    let mut graph_rev = snap.graph_revision;
    for (up, down, label) in [
        (&build_eid, &unit_eid, "build->unit"),
        (&build_eid, &integ_eid, "build->integ"),
        (&build_eid, &e2e_eid, "build->e2e"),
        (&unit_eid, &deploy_eid, "unit->deploy"),
        (&integ_eid, &deploy_eid, "integ->deploy"),
        (&e2e_eid, &deploy_eid, "e2e->deploy"),
        (&deploy_eid, &verify_eid, "deploy->verify"),
    ] {
        graph_rev = stage_and_apply(&http, &args, &flow_id, up, down, graph_rev).await?;
        println!("[timeline] edge_applied {label}");
    }

    println!(
        "approval waitpoint_key={approval_wp_key}\n  approve with: cargo run --bin approve -- --flow-id {flow_id} --reviewer <name>"
    );

    // Poll verify state.
    let state_url = format!("{}/v1/executions/{}/state", args.server, verify_eid);
    let mut last = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    loop {
        if std::time::Instant::now() > deadline {
            println!("[timeline] submit_timeout");
            break;
        }
        tokio::time::sleep(Duration::from_secs(args.poll_secs)).await;
        let mut req = http.get(&state_url);
        if let Some(t) = &args.api_token {
            req = req.bearer_auth(t);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let state = resp
            .json::<serde_json::Value>()
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned();
        if state != last {
            println!("[verify state] {state}");
            last = state.clone();
        }
        if matches!(state.as_str(), "completed" | "failed" | "cancelled") {
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn create_exec(
    http: &reqwest::Client,
    args: &Args,
    eid: &ExecutionId,
    partition: u16,
    kind: &str,
    lane: &str,
    payload: &[u8],
    caps: &BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": args.namespace,
        "lane_id": lane,
        "execution_kind": kind,
        "input_payload": payload,
        "payload_encoding": "json",
        "priority": 100,
        "creator_identity": "deploy-submit",
        "tags": {},
        "policy": {
            "routing_requirements": { "required_capabilities": caps }
        },
        "partition_id": partition,
        "now": TimestampMs::now().0,
    });
    http_post(http, args, &format!("{}/v1/executions", args.server), &body).await?;
    Ok(())
}

async fn add_member(
    http: &reqwest::Client,
    args: &Args,
    flow_id: &FlowId,
    eid: &ExecutionId,
) -> Result<(), Box<dyn std::error::Error>> {
    http_post(
        http,
        args,
        &format!("{}/v1/flows/{}/members", args.server, flow_id),
        &serde_json::json!({
            "flow_id": flow_id,
            "execution_id": eid,
            "now": TimestampMs::now().0,
        }),
    )
    .await?;
    Ok(())
}

async fn stage_and_apply(
    http: &reqwest::Client,
    args: &Args,
    flow_id: &FlowId,
    up: &ExecutionId,
    down: &ExecutionId,
    expected_rev: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let edge_id = EdgeId::new();
    let stage = http_post(
        http,
        args,
        &format!("{}/v1/flows/{}/edges", args.server, flow_id),
        &serde_json::json!({
            "flow_id": flow_id,
            "edge_id": edge_id,
            "upstream_execution_id": up,
            "downstream_execution_id": down,
            "dependency_kind": "success_only",
            "expected_graph_revision": expected_rev,
            "now": TimestampMs::now().0,
        }),
    )
    .await?;
    let new_rev = stage
        .get("Staged")
        .and_then(|v| v.get("new_graph_revision"))
        .and_then(|v| v.as_u64())
        .ok_or("stage edge: missing new_graph_revision")?;
    http_post(
        http,
        args,
        &format!("{}/v1/flows/{}/edges/apply", args.server, flow_id),
        &serde_json::json!({
            "flow_id": flow_id,
            "edge_id": edge_id,
            "downstream_execution_id": down,
            "upstream_execution_id": up,
            "graph_revision": new_rev,
            "dependency_kind": "success_only",
            "now": TimestampMs::now().0,
        }),
    )
    .await?;
    Ok(new_rev)
}

async fn http_post(
    http: &reqwest::Client,
    args: &Args,
    url: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut req = http.post(url).json(body);
    if let Some(t) = &args.api_token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("POST {url} -> {status}: {text}").into());
    }
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
}
