//! Scenario 7 — 100-way Quorum(1)+CancelRemaining fanout SLO.
//!
//! Master spec §4.2: p99 end-to-end dispatch latency MUST stay within
//! 500ms at n=100. Wave 7c measured 313ms on Postgres; this smoke
//! asserts the same gate as a release check.
//!
//! Shape (per sample): create flow with 100 upstream members + 1
//! downstream, stage 100 AnyOf/Quorum(1)+CancelRemaining edges, apply
//! them, drive upstream[0] to `terminal`, call `dispatch_completion`
//! (this flips `cancel_siblings_pending_flag=TRUE` — the "flag-set
//! moment" for latency measurement), then call `dispatcher_tick`
//! repeatedly until all 99 siblings reach `public_state='cancelled'`.
//! Record wall time from flag-set to last-sibling-observed.
//!
//! 10 samples minimum, p99 ≤ 500 ms.
//!
//! The Valkey leg stays as a reachability probe (see v0.7 smoke Wave
//! 7b) — the SLO measurement is Postgres-specific in this pass.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ff_backend_postgres::{PgPool, PostgresBackend};
use ff_backend_postgres::reconcilers::edge_cancel_dispatcher;
use ff_core::backend::ScannerFilter;
use ff_core::contracts::{
    AddExecutionToFlowArgs, ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::{EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs};

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "fanout_slo";
const VALKEY_FANOUT: usize = 50;
const VALKEY_DEADLINE_SECS: u64 = 30;

// Postgres SLO measurement knobs (master spec §4.2).
const PG_FANIN: usize = 100;
const PG_SAMPLES: usize = 10;
const PG_P99_SLO_MS: u128 = 500;
const PG_DISPATCH_DEADLINE: Duration = Duration::from_secs(30);

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    let mut seeded: Vec<ff_core::types::ExecutionId> = Vec::with_capacity(VALKEY_FANOUT);
    for _ in 0..VALKEY_FANOUT {
        let (_fid, eid) = scenarios::mint_fresh_exec_id(&ctx.partition_config);
        if let Err(e) = scenarios::http_create_execution(&ctx, &eid, b"fanout".to_vec()).await {
            return ScenarioReport::fail(
                NAME,
                "valkey",
                started,
                format!("create[{}]: {e}", seeded.len()),
            );
        }
        seeded.push(eid);
    }
    // All seeded — confirm a sample of their state endpoints answer.
    for eid in seeded.iter().take(5) {
        let url = format!("{}/v1/executions/{}/state", ctx.http_server, eid);
        match ctx.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                return ScenarioReport::fail(
                    NAME,
                    "valkey",
                    started,
                    format!("state[{eid}] HTTP {}", resp.status()),
                );
            }
            Err(e) => {
                return ScenarioReport::fail(NAME, "valkey", started, format!("state: {e}"));
            }
        }
    }
    let _ = VALKEY_DEADLINE_SECS;
    ScenarioReport::pass(NAME, "valkey", started)
}

pub async fn run_postgres(
    ctx: Arc<SmokeCtx>,
    pg: Arc<PostgresBackend>,
    backend: Arc<dyn EngineBackend>,
    pool: PgPool,
) -> ScenarioReport {
    let _ = backend;
    let started = scenarios::start();
    let mut samples_ms: Vec<u128> = Vec::with_capacity(PG_SAMPLES);

    for sample in 0..PG_SAMPLES {
        match run_one_sample(&ctx, &pg, &pool, sample).await {
            Ok(ms) => samples_ms.push(ms),
            Err(SampleErr::Unavailable(op)) => {
                return ScenarioReport::skip(
                    NAME,
                    "postgres",
                    format!("Unavailable at sample {sample}: {op}"),
                );
            }
            Err(SampleErr::Other(msg)) => {
                return ScenarioReport::fail(NAME, "postgres", started, msg);
            }
        }
    }

    samples_ms.sort_unstable();
    let p50 = samples_ms[samples_ms.len() / 2];
    let p99_idx = ((samples_ms.len() as f64) * 0.99).ceil() as usize - 1;
    let p99 = samples_ms[p99_idx.min(samples_ms.len() - 1)];
    let max = *samples_ms.last().unwrap_or(&0);
    tracing::info!(
        scenario = NAME,
        backend = "postgres",
        samples = samples_ms.len(),
        p50_ms = p50,
        p99_ms = p99,
        max_ms = max,
        slo_p99_ms = PG_P99_SLO_MS,
        "fanout_slo postgres latencies"
    );

    if p99 > PG_P99_SLO_MS {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!(
                "p99={p99}ms exceeds SLO {PG_P99_SLO_MS}ms (p50={p50}, max={max}, n={fanin}, samples={samples})",
                fanin = PG_FANIN,
                samples = samples_ms.len(),
            ),
        );
    }
    ScenarioReport::pass(NAME, "postgres", started)
}

enum SampleErr {
    Unavailable(&'static str),
    Other(String),
}

impl From<sqlx::Error> for SampleErr {
    fn from(e: sqlx::Error) -> Self {
        SampleErr::Other(format!("sqlx: {e}"))
    }
}

impl From<EngineError> for SampleErr {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Unavailable { op } => SampleErr::Unavailable(op),
            other => SampleErr::Other(format!("engine: {other}")),
        }
    }
}

async fn run_one_sample(
    ctx: &Arc<SmokeCtx>,
    pg: &Arc<PostgresBackend>,
    pool: &PgPool,
    sample_idx: usize,
) -> Result<u128, SampleErr> {
    let pc = ctx.partition_config;
    let lane = LaneId::new(scenarios::SMOKE_LANE);
    let namespace = Namespace::new(scenarios::SMOKE_NAMESPACE);
    let now = TimestampMs::from_millis(scenarios::now_ms());

    let flow = FlowId::new();
    pg.create_flow(&CreateFlowArgs {
        flow_id: flow.clone(),
        flow_kind: "smoke-fanout".to_owned(),
        namespace: namespace.clone(),
        now,
    })
    .await?;

    let upstreams: Vec<ExecutionId> =
        (0..PG_FANIN).map(|_| ExecutionId::for_flow(&flow, &pc)).collect();
    let downstream = ExecutionId::for_flow(&flow, &pc);

    for eid in upstreams.iter().chain(std::iter::once(&downstream)) {
        let args = CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: namespace.clone(),
            lane_id: lane.clone(),
            execution_kind: scenarios::SMOKE_KIND.to_string(),
            input_payload: Vec::new(),
            payload_encoding: Some("binary".to_string()),
            priority: 100,
            creator_identity: "ff-smoke-v0.7".to_string(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: eid.partition(),
            now,
        };
        pg.create_execution(args).await?;
        pg.add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow.clone(),
            execution_id: eid.clone(),
            now,
        })
        .await?;
    }

    pg.set_edge_group_policy(
        &flow,
        &downstream,
        EdgeDependencyPolicy::quorum(1, OnSatisfied::cancel_remaining()),
    )
    .await?;

    // graph_revision: create_flow seeds rev=0; each fresh
    // add_execution_to_flow bumps by 1. PG_FANIN upstreams +
    // 1 downstream = PG_FANIN+1 adds.
    let mut rev: u64 = (PG_FANIN as u64) + 1;
    for up in &upstreams {
        let edge_id = EdgeId::new();
        let staged = pg
            .stage_dependency_edge(&StageDependencyEdgeArgs {
                flow_id: flow.clone(),
                edge_id: edge_id.clone(),
                upstream_execution_id: up.clone(),
                downstream_execution_id: downstream.clone(),
                dependency_kind: "success_only".to_string(),
                data_passing_ref: None,
                expected_graph_revision: rev,
                now,
            })
            .await?;
        let StageDependencyEdgeResult::Staged { new_graph_revision, .. } = staged;
        rev = new_graph_revision;
        pg.apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: flow.clone(),
            edge_id,
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: up.clone(),
            graph_revision: rev,
            dependency_kind: "success_only".to_string(),
            data_passing_ref: None,
            now,
        })
        .await?;
    }

    // Drive upstream[0] → terminal.
    let part_i16 = ff_core::partition::flow_partition(&flow, &pc).index as i16;
    let first_up_uuid = parse_exec_uuid(&upstreams[0]);
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal', \
            public_state = 'completed', terminal_at_ms = $3 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(now.0)
    .execute(pool)
    .await?;

    let event_id: i64 = sqlx::query_scalar(
        "INSERT INTO ff_completion_event \
            (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
         VALUES ($1, $2, $3, 'success', $4) RETURNING event_id",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(flow.0)
    .bind(now.0)
    .fetch_one(pool)
    .await?;

    // ── Flag-set moment: dispatch_completion flips the cancel flag.
    let flag_set_at = Instant::now();
    ff_backend_postgres::dispatch::dispatch_completion(pool, event_id).await?;

    // Drain until all 99 siblings are cancelled (or deadline).
    let expected_cancelled = (PG_FANIN - 1) as i64;
    let down_uuid = parse_exec_uuid(&downstream);
    let deadline = Instant::now() + PG_DISPATCH_DEADLINE;
    loop {
        edge_cancel_dispatcher::dispatcher_tick(pool, &ScannerFilter::new()).await?;
        let cancelled: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ff_exec_core \
             WHERE partition_key = $1 AND flow_id = $2 \
               AND public_state = 'cancelled' \
               AND execution_id <> $3",
        )
        .bind(part_i16)
        .bind(flow.0)
        .bind(down_uuid)
        .fetch_one(pool)
        .await?;
        if cancelled >= expected_cancelled {
            break;
        }
        if Instant::now() > deadline {
            return Err(SampleErr::Other(format!(
                "sample {sample_idx}: only {cancelled}/{expected_cancelled} siblings cancelled within {}s",
                PG_DISPATCH_DEADLINE.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(flag_set_at.elapsed().as_millis())
}

fn parse_exec_uuid(eid: &ExecutionId) -> uuid::Uuid {
    let s = eid.as_str();
    let colon = s.rfind("}:").expect("hash-tag delimiter");
    uuid::Uuid::parse_str(&s[colon + 2..]).expect("uuid suffix")
}
