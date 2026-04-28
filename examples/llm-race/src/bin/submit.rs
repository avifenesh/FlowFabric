//! UC-38 `submit` CLI.
//!
//! Wires up the race flow:
//!   1. Create flow.
//!   2. Discover up to 3 free OpenRouter models.
//!   3. Create one `provider` execution per model (flow-member, solo partition on
//!      the aggregator's flow partition so edges co-locate — RFC-011).
//!   4. Create one `aggregator` execution (flow-member).
//!   5. Set `AnyOf { CancelRemaining }` edge-group policy on the aggregator
//!      (RFC-016 Stage B).
//!   6. Stage + apply one dependency edge from each provider to the aggregator
//!      (RFC-007 + RFC-016).
//!   7. Poll the aggregator's state and tail its `DurableSummary` stream
//!      (RFC-015). Print a timeline of claim/complete/cancel/frame events so
//!      CancelRemaining is visible.
//!
//! NOT runnable until ff 0.6.1 ships the `read_summary` decoder fix —
//! see the example's README.

use std::collections::BTreeSet;
use std::time::Duration;

use clap::Parser;
use ff_core::contracts::{EdgeDependencyPolicy, OnSatisfied};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::{EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId};
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use llm_race_example::{
    discover_free_models, AggregatorPayload, ProviderPayload, CAP_ROLE_AGGREGATOR,
    CAP_ROLE_PROVIDER, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL, EXECUTION_KIND_AGGREGATOR,
    EXECUTION_KIND_PROVIDER, LANE_AGGREGATOR, LANE_PROVIDER,
};

#[derive(Parser)]
#[command(name = "submit", about = "Submit a UC-38 model-race flow")]
struct Args {
    /// FlowFabric REST server URL.
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,

    /// Valkey host (used only for the one SDK call:
    /// `set_edge_group_policy`, which has no REST route yet).
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,

    /// Valkey port.
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,

    /// Prompt to send to every racer.
    #[arg(long)]
    prompt: String,

    /// Max tokens per provider response.
    #[arg(long, default_value_t = 512)]
    max_tokens: u32,

    /// FlowFabric namespace.
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,

    /// Number of flow partitions the server is configured with.
    #[arg(long, default_value_t = 256)]
    flow_partitions: u16,

    /// Optional bearer token for ff-server.
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,

    /// Enable 2-of-3 HITL review on the aggregator before the final
    /// summary is published. Wires a `Count(2, DistinctSources)`
    /// composite condition.
    #[arg(long)]
    review: bool,

    /// Poll cadence for aggregator state.
    #[arg(long, default_value_t = 2)]
    poll_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_race_example=info,submit=info".into()),
        )
        .init();

    let args = Args::parse();

    // ── 1. Discover free models ──
    let http = reqwest::Client::new();
    let free = discover_free_models(&http).await?;
    if free.len() < 2 {
        eprintln!(
            "only {} free model(s) on OpenRouter right now — AnyOf race needs >= 2.",
            free.len()
        );
        std::process::exit(1);
    }
    let chosen: Vec<_> = free.into_iter().take(3).collect();
    println!(
        "Racing {} free model(s):",
        chosen.len()
    );
    for m in &chosen {
        println!(
            "  - {} ({})",
            m.id,
            m.name.as_deref().unwrap_or("unnamed")
        );
    }

    // ── 2. Mint flow + execution ids ──
    let partition_config = PartitionConfig {
        num_flow_partitions: args.flow_partitions,
        ..PartitionConfig::default()
    };
    let flow_id = FlowId::new();
    let partition = flow_partition(&flow_id, &partition_config).index;

    let aggregator_eid = ExecutionId::for_flow(&flow_id, &partition_config);
    let provider_eids: Vec<_> = chosen
        .iter()
        .map(|_| ExecutionId::for_flow(&flow_id, &partition_config))
        .collect();

    // ── 3. REST: create flow ──
    let now_ms = TimestampMs::now().0;
    http_post_json(
        &http,
        &args,
        &format!("{}/v1/flows", args.server),
        &serde_json::json!({
            "flow_id": flow_id,
            "flow_kind": "llm_race",
            "namespace": args.namespace,
            "now": now_ms,
        }),
    )
    .await?;
    println!("Flow created: {}", flow_id);

    // ── 4. REST: create provider executions ──
    for (eid, model) in provider_eids.iter().zip(chosen.iter()) {
        let payload = ProviderPayload {
            prompt: args.prompt.clone(),
            model: model.id.clone(),
            max_tokens: args.max_tokens,
        };
        create_execution(
            &http,
            &args,
            eid,
            partition,
            EXECUTION_KIND_PROVIDER,
            LANE_PROVIDER,
            &serde_json::to_vec(&payload)?,
            &BTreeSet::from([CAP_ROLE_PROVIDER.to_owned()]),
        )
        .await?;
        // Add to flow
        add_execution_to_flow(&http, &args, &flow_id, eid).await?;
        println!("Provider execution created: {} model={}", eid, model.id);
    }

    // ── 5. REST: create aggregator ──
    let review_wp_key = if args.review {
        Some(format!("wpk:review:{}", uuid::Uuid::new_v4()))
    } else {
        None
    };
    let agg_payload = AggregatorPayload {
        prompt: args.prompt.clone(),
        review_waitpoint_key: review_wp_key.clone(),
    };
    create_execution(
        &http,
        &args,
        &aggregator_eid,
        partition,
        EXECUTION_KIND_AGGREGATOR,
        LANE_AGGREGATOR,
        &serde_json::to_vec(&agg_payload)?,
        &BTreeSet::from([CAP_ROLE_AGGREGATOR.to_owned()]),
    )
    .await?;
    add_execution_to_flow(&http, &args, &flow_id, &aggregator_eid).await?;
    println!("Aggregator execution created: {}", aggregator_eid);

    // ── 6. SDK: set AnyOf { CancelRemaining } on the aggregator ──
    // RFC-016 Stage B requires the policy be set BEFORE the first
    // `add_dependency(... -> aggregator)` edge lands.
    let sdk = FlowFabricWorker::connect(WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: WorkerId::new("llm-race-submit"),
        worker_instance_id: WorkerInstanceId::new(format!(
            "llm-race-submit-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: Namespace::new(&args.namespace),
        lanes: vec![LaneId::new(LANE_AGGREGATOR)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
    })
    .await?;
    sdk.set_edge_group_policy(
        &flow_id,
        &aggregator_eid,
        EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
    )
    .await?;
    println!("Edge-group policy set: AnyOf {{ CancelRemaining }}");

    // ── 7. REST: stage + apply one edge per provider → aggregator ──
    // stage_dependency_edge returns the new graph revision; we pass
    // that to apply_dependency_to_child which hangs the dep off the
    // downstream child counter. The initial expected revision must
    // match the current flow_core state (bumped on every add-member),
    // so fetch it via the SDK before the first stage.
    let snap = sdk
        .describe_flow(&flow_id)
        .await?
        .ok_or("describe_flow returned None right after create_flow")?;
    let mut graph_rev = snap.graph_revision;
    for upstream_eid in &provider_eids {
        let edge_id = EdgeId::new();
        let stage_res = http_post_json(
            &http,
            &args,
            &format!("{}/v1/flows/{}/edges", args.server, flow_id),
            &serde_json::json!({
                "flow_id": flow_id,
                "edge_id": edge_id,
                "upstream_execution_id": upstream_eid,
                "downstream_execution_id": aggregator_eid,
                "dependency_kind": "success_only",
                "expected_graph_revision": graph_rev,
                "now": TimestampMs::now().0,
            }),
        )
        .await?;
        graph_rev = stage_res
            .get("Staged")
            .and_then(|v| v.get("new_graph_revision"))
            .and_then(|v| v.as_u64())
            .ok_or("stage_dependency_edge: missing new_graph_revision")?;

        http_post_json(
            &http,
            &args,
            &format!("{}/v1/flows/{}/edges/apply", args.server, flow_id),
            &serde_json::json!({
                "flow_id": flow_id,
                "edge_id": edge_id,
                "downstream_execution_id": aggregator_eid,
                "upstream_execution_id": upstream_eid,
                "graph_revision": graph_rev,
                "dependency_kind": "success_only",
                "now": TimestampMs::now().0,
            }),
        )
        .await?;
        println!("Edge staged+applied: {} -> {}", upstream_eid, aggregator_eid);
    }

    if let Some(k) = &review_wp_key {
        println!(
            "HITL enabled — aggregator will suspend on waitpoint_key={k}\n  \
             approve with: cargo run --bin approve -- --execution-id {} --waitpoint-key {} --reviewer <name>",
            aggregator_eid, k
        );
    }

    // ── 8. Poll aggregator state for the demo timeline ──
    let state_url = format!("{}/v1/executions/{}/state", args.server, aggregator_eid);
    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(args.poll_secs)).await;
        let resp = http.get(&state_url).send().await?;
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
            println!("[aggregator state] {state}");
            last = state.clone();
        }
        if matches!(state.as_str(), "completed" | "failed" | "cancelled") {
            break;
        }
    }
    Ok(())
}

// ── helpers ──

async fn create_execution(
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
        "creator_identity": "llm-race-submit",
        "tags": {},
        "policy": {
            "required_capabilities": caps,
        },
        "partition_id": partition,
        "now": TimestampMs::now().0,
    });
    http_post_json(
        http,
        args,
        &format!("{}/v1/executions", args.server),
        &body,
    )
    .await?;
    Ok(())
}

async fn add_execution_to_flow(
    http: &reqwest::Client,
    args: &Args,
    flow_id: &FlowId,
    eid: &ExecutionId,
) -> Result<(), Box<dyn std::error::Error>> {
    http_post_json(
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

async fn http_post_json(
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
