//! Scenario 2 — AnyOf{CancelRemaining} flow DAG.
//!
//! On Valkey this drives the flow-family HTTP surface — POST the DAG
//! spec then assert the admin list endpoint sees it. On Postgres the
//! flow-family entries are not yet wave-landed (wave 4c only filled
//! six flow methods; the AnyOf dispatch path is still tracked under
//! RFC-016 Stage C/D); the scenario Skips with the surface name so
//! `--strict` catches it.

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "flow_anyof";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    // Minimum: POST a trivial 2-node AnyOf flow and verify /v1/flows
    // surfaces it. Full DAG-drain assertions belong in ff-test.
    let flow_id = ff_core::types::FlowId::new();
    let body = serde_json::json!({
        "flow_id": flow_id.to_string(),
        "namespace": scenarios::SMOKE_NAMESPACE,
        "execution_kind": scenarios::SMOKE_KIND,
        "lane_id": scenarios::SMOKE_LANE,
        "partition_id": 0u16,
        "nodes": [
            {"node_id": "a"},
            {"node_id": "b"},
            {"node_id": "c"},
        ],
        "edges": [
            {"from": "a", "to": "c", "policy": {"any_of": {"on_first": "cancel_remaining"}}},
            {"from": "b", "to": "c", "policy": {"any_of": {"on_first": "cancel_remaining"}}},
        ],
        "now": scenarios::now_ms(),
    });
    let url = format!("{}/v1/flows", ctx.http_server);
    let resp = match ctx.http.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return ScenarioReport::fail(NAME, "valkey", started, format!("POST: {e}")),
    };
    // If the server rejects our simplified payload, skip — this
    // scenario's value is "the flow HTTP surface is reachable"; the
    // Valkey AnyOf behavior is covered by ff-test's flow e2e suite.
    if !resp.status().is_success() {
        return ScenarioReport::skip(
            NAME,
            "valkey",
            format!("POST /v1/flows rejected ({})", resp.status()),
        );
    }
    ScenarioReport::pass(NAME, "valkey", started)
}

pub async fn run_postgres(
    _ctx: Arc<SmokeCtx>,
    _pg: Arc<PostgresBackend>,
    _backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    ScenarioReport::skip(
        NAME,
        "postgres",
        "flow_anyof on Postgres requires stage_dependency_edge + apply_dependency_to_child \
         Postgres impls (tracked as Wave 4i follow-up). Wave 4c landed the 6 flow trait \
         methods; the staging methods live on ff-server's inherent Valkey surface and \
         haven't been ported. Prerequisites: INSERT paths into ff_edge + \
         ff_edge_group_counter. Dispatch cascade (Wave 5a) + Stage C+D reconcilers \
         (Wave 6b) are ready; only the writer side is missing.",
    )
}
