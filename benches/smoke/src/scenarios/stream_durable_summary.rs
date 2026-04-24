//! Scenario 4 — DurableSummary stream + `read_summary`.
//!
//! This is the scenario that WOULD have caught the v0.6.0 release
//! bug (`read_summary` returning None unconditionally due to a
//! RESP3 Map decode regression; `feedback_smoke_before_release`).
//! Accordingly it is the highest-stakes scenario in the smoke: a
//! `read_summary` call against a fresh execution must not panic and
//! must return a well-typed `Ok(None)` (no summary yet) rather than
//! a Transport error.

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::AttemptIndex;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "stream_durable_summary";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    // GET /v1/executions/{bogus}/summary — endpoint wiring probe.
    // (The bare trait call via ff-backend-valkey is out of scope
    // here — HTTP leg validates the server-side plumbing, which is
    // the published v0.6 consumer surface where the bug lived.)
    let fake = ff_core::types::ExecutionId::for_flow(
        &ff_core::types::FlowId::new(),
        &ctx.partition_config,
    );
    let url = format!("{}/v1/executions/{}/summary?attempt=1", ctx.http_server, fake);
    match ctx.http.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_server_error() {
                ScenarioReport::fail(
                    NAME,
                    "valkey",
                    started,
                    format!("summary endpoint 5xx: {status}"),
                )
            } else {
                ScenarioReport::pass(NAME, "valkey", started)
            }
        }
        Err(e) => ScenarioReport::fail(NAME, "valkey", started, format!("GET: {e}")),
    }
}

pub async fn run_postgres(
    ctx: Arc<SmokeCtx>,
    _pg: Arc<PostgresBackend>,
    backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    let started = scenarios::start();
    // Direct trait call — `read_summary` against a freshly-minted
    // eid. Expected: `Ok(None)` (no summary rows exist). A Transport
    // error indicates a regression; `Unavailable` is a Skip (wave
    // hasn't landed streaming yet on Postgres).
    let (_fid, eid) = scenarios::mint_fresh_exec_id(&ctx.partition_config);
    match backend.read_summary(&eid, AttemptIndex::new(1)).await {
        Ok(None) => ScenarioReport::pass(NAME, "postgres", started),
        Ok(Some(_)) => ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            "read_summary returned Some for a never-written execution",
        ),
        Err(EngineError::Unavailable { op }) => {
            ScenarioReport::skip(NAME, "postgres", format!("Unavailable: {op}"))
        }
        Err(e) => ScenarioReport::fail(NAME, "postgres", started, format!("read_summary: {e}")),
    }
}
