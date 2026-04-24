//! Scenario 5 — BestEffortLive dynamic MAXLEN probe.
//!
//! The RFC-015 dynamic-MAXLEN algorithm lives server-side and on the
//! Valkey backend's `append_frame` path. The smoke's job here is
//! narrow: prove the `read_stream` / `tail_stream` methods on the
//! trait route without error against a fresh execution. Regressions
//! in the BestEffortLive config codec (another RESP3 Map decode
//! candidate, parallel to v0.6's `read_summary` bug) show up as
//! Transport faults here.

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::AttemptIndex;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "stream_best_effort";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    let fake = ff_core::types::ExecutionId::for_flow(
        &ff_core::types::FlowId::new(),
        &ctx.partition_config,
    );
    let url = format!(
        "{}/v1/executions/{}/stream?attempt=1&mode=best_effort_live",
        ctx.http_server, fake
    );
    match ctx.http.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_server_error() {
                ScenarioReport::fail(
                    NAME,
                    "valkey",
                    started,
                    format!("stream endpoint 5xx: {status}"),
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
    let (_fid, eid) = scenarios::mint_fresh_exec_id(&ctx.partition_config);
    // `read_stream` from id 0 with a tiny budget. Empty result on a
    // fresh execution is the expected Ok outcome.
    let from = ff_core::contracts::StreamCursor::Start;
    let to = ff_core::contracts::StreamCursor::End;
    match backend
        .read_stream(&eid, AttemptIndex::new(1), from, to, 16)
        .await
    {
        Ok(_) => ScenarioReport::pass(NAME, "postgres", started),
        Err(EngineError::Unavailable { op }) => {
            ScenarioReport::skip(NAME, "postgres", format!("Unavailable: {op}"))
        }
        Err(e) => ScenarioReport::fail(NAME, "postgres", started, format!("read_stream: {e}")),
    }
}
