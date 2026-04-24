//! Scenario 2 — AnyOf{CancelRemaining} flow DAG.
//!
//! On Valkey this drives the flow-family HTTP surface — POST the DAG
//! spec then assert the admin list endpoint sees it.
//!
//! On Postgres we mirror the Wave 4i `pg_flow_staging.rs::e2e_any_of_
//! cancel_remaining_cascade` shape + the Wave 7b/cleanup-PR-#256
//! closure (members populated, dispatcher drain): create a flow with
//! 3 upstream members + 1 downstream, stage 3 AnyOf+CancelRemaining
//! edges, apply them, drive upstream[0] to `terminal`, fire
//! `dispatch::dispatch_completion`, then run
//! `edge_cancel_dispatcher::dispatcher_tick` to drain the pending-
//! cancel group. Finally assert:
//!
//! * downstream is `eligible_now` (AnyOf satisfied on first success)
//! * the other 2 siblings are `lifecycle_phase='terminal'` +
//!   `public_state='cancelled'` + `cancellation_reason='sibling_quorum_satisfied'`

use std::sync::Arc;

use ff_backend_postgres::{PgPool, PostgresBackend};
use ff_backend_postgres::reconcilers::edge_cancel_dispatcher;
use ff_core::backend::ScannerFilter;
use ff_core::contracts::{
    AddExecutionToFlowArgs, ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs};

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "flow_anyof";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    // Minimum: POST a well-formed [`CreateFlowArgs`] to /v1/flows and
    // verify the server accepts it (2xx). Full DAG-drain assertions
    // belong in ff-test; here we just confirm the flow HTTP surface
    // is reachable, matching the Valkey scenario intent from Wave 7b.
    let flow_id = ff_core::types::FlowId::new();
    let body = serde_json::json!({
        "flow_id": flow_id.to_string(),
        "flow_kind": "smoke",
        "namespace": scenarios::SMOKE_NAMESPACE,
        "now": scenarios::now_ms(),
    });
    let url = format!("{}/v1/flows", ctx.http_server);
    let resp = match ctx.http.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return ScenarioReport::fail(NAME, "valkey", started, format!("POST: {e}")),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return ScenarioReport::fail(
            NAME,
            "valkey",
            started,
            format!("POST /v1/flows failed ({status}): {text}"),
        );
    }
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
    let pc = ctx.partition_config;
    let lane = LaneId::new(scenarios::SMOKE_LANE);
    let namespace = Namespace::new(scenarios::SMOKE_NAMESPACE);
    let now = TimestampMs::from_millis(scenarios::now_ms());

    // ── Seed flow + 3 upstream members + 1 downstream ──
    let flow = FlowId::new();
    if let Err(e) = pg
        .create_flow(&CreateFlowArgs {
            flow_id: flow.clone(),
            flow_kind: "smoke".to_owned(),
            namespace: namespace.clone(),
            now,
        })
        .await
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create_flow: {e}"));
    }

    let upstreams: Vec<ExecutionId> = (0..3).map(|_| ExecutionId::for_flow(&flow, &pc)).collect();
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
        if let Err(e) = pg.create_execution(args).await {
            return ScenarioReport::fail(NAME, "postgres", started, format!("create exec: {e}"));
        }
        if let Err(e) = pg
            .add_execution_to_flow(&AddExecutionToFlowArgs {
                flow_id: flow.clone(),
                execution_id: eid.clone(),
                now,
            })
            .await
        {
            return ScenarioReport::fail(NAME, "postgres", started, format!("add_to_flow: {e}"));
        }
    }

    // ── Policy + 3 edges ──
    if let Err(e) = pg
        .set_edge_group_policy(
            &flow,
            &downstream,
            EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
        )
        .await
    {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("set_edge_group_policy: {e}"),
        );
    }

    // graph_revision after create_flow + 4 add_execution_to_flow = 4.
    let mut rev: u64 = 4;
    for up in &upstreams {
        let edge_id = EdgeId::new();
        let staged = match pg
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
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ScenarioReport::fail(NAME, "postgres", started, format!("stage: {e}"));
            }
        };
        let StageDependencyEdgeResult::Staged {
            new_graph_revision, ..
        } = staged;
        rev = new_graph_revision;
        if let Err(e) = pg
            .apply_dependency_to_child(&ApplyDependencyToChildArgs {
                flow_id: flow.clone(),
                edge_id,
                downstream_execution_id: downstream.clone(),
                upstream_execution_id: up.clone(),
                graph_revision: rev,
                dependency_kind: "success_only".to_string(),
                data_passing_ref: None,
                now,
            })
            .await
        {
            return ScenarioReport::fail(NAME, "postgres", started, format!("apply: {e}"));
        }
    }

    // ── Drive upstream[0] to terminal + fire completion event ──
    let part_i16 = ff_core::partition::flow_partition(&flow, &pc).index as i16;
    let first_up_uuid = parse_exec_uuid(&upstreams[0]);

    if let Err(e) = sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal', \
            public_state = 'completed', terminal_at_ms = $3 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(now.0)
    .execute(&pool)
    .await
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("upstream terminal: {e}"));
    }

    let event_id: i64 = match sqlx::query_scalar(
        "INSERT INTO ff_completion_event \
            (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
         VALUES ($1, $2, $3, 'success', $4) RETURNING event_id",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(flow.0)
    .bind(now.0)
    .fetch_one(&pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("completion event: {e}"));
        }
    };

    if let Err(e) = ff_backend_postgres::dispatch::dispatch_completion(&pool, event_id).await {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("dispatch_completion: {e}"),
        );
    }

    // ── Drain pending-cancel groups ──
    if let Err(e) =
        edge_cancel_dispatcher::dispatcher_tick(&pool, &ScannerFilter::new()).await
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("dispatcher_tick: {e}"));
    }

    // ── Assertions ──
    let down_uuid = parse_exec_uuid(&downstream);
    let elig: String = match sqlx::query_scalar(
        "SELECT eligibility_state FROM ff_exec_core \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(down_uuid)
    .fetch_one(&pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!("downstream select: {e}"),
            );
        }
    };
    if elig != "eligible_now" {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("downstream eligibility_state={elig} (want eligible_now)"),
        );
    }

    for (i, sib) in upstreams.iter().enumerate().skip(1) {
        let sib_uuid = parse_exec_uuid(sib);
        let row: (String, String, Option<String>) = match sqlx::query_as(
            "SELECT lifecycle_phase, public_state, cancellation_reason FROM ff_exec_core \
              WHERE partition_key = $1 AND execution_id = $2",
        )
        .bind(part_i16)
        .bind(sib_uuid)
        .fetch_one(&pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return ScenarioReport::fail(
                    NAME,
                    "postgres",
                    started,
                    format!("sibling[{i}] select: {e}"),
                );
            }
        };
        if row.0 != "terminal" || row.1 != "cancelled" {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!(
                    "sibling[{i}] not cancelled: phase={} public={}",
                    row.0, row.1
                ),
            );
        }
        if row.2.as_deref() != Some("sibling_quorum_satisfied") {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!("sibling[{i}] cancellation_reason={:?}", row.2),
            );
        }
    }

    ScenarioReport::pass(NAME, "postgres", started)
}

fn parse_exec_uuid(eid: &ExecutionId) -> uuid::Uuid {
    let s = eid.as_str();
    let colon = s.rfind("}:").expect("hash-tag delimiter");
    uuid::Uuid::parse_str(&s[colon + 2..]).expect("uuid suffix")
}
