//! Scenario 6 — cancel cascade through dispatcher + reconciler.
//!
//! The edge_cancel_dispatcher + reconciler pair is v0.7-Wave-6b/c
//! work (Postgres only). A full cascade assertion needs a live
//! engine spinning the dispatcher loop — out of scope for a pre-
//! release smoke. Scope here: prove the `cancel_flow` / `cancel`
//! trait entry points route without transport faults.

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "cancel_cascade";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    // POST /v1/executions/{bogus}/cancel — HTTP wiring probe.
    let fake = ff_core::types::ExecutionId::for_flow(
        &ff_core::types::FlowId::new(),
        &ctx.partition_config,
    );
    let url = format!("{}/v1/executions/{}/cancel", ctx.http_server, fake);
    let body = serde_json::json!({
        "reason": "smoke-probe",
        "now": scenarios::now_ms(),
    });
    match ctx.http.post(&url).json(&body).send().await {
        Ok(resp) => {
            if resp.status().is_server_error() {
                ScenarioReport::fail(
                    NAME,
                    "valkey",
                    started,
                    format!("cancel endpoint 5xx: {}", resp.status()),
                )
            } else {
                ScenarioReport::pass(NAME, "valkey", started)
            }
        }
        Err(e) => ScenarioReport::fail(NAME, "valkey", started, format!("POST: {e}")),
    }
}

pub async fn run_postgres(
    ctx: Arc<SmokeCtx>,
    pg: Arc<PostgresBackend>,
    backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    let started = scenarios::start();
    // Seed → claim → cancel. Any routed outcome on `cancel` is Pass;
    // Transport is Fail; Unavailable is Skip.
    let (_eid, args) = scenarios::build_create_args(&ctx.partition_config);
    if let Err(e) = pg.create_execution(args).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create: {e}"));
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let handle = match scenarios::claim_one(&backend).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return ScenarioReport::skip(NAME, "postgres", "claim returned Ok(None)");
        }
        Err(EngineError::Unavailable { op }) => {
            return ScenarioReport::skip(NAME, "postgres", format!("Unavailable: {op}"));
        }
        Err(e) => return ScenarioReport::fail(NAME, "postgres", started, format!("claim: {e}")),
    };
    match backend.cancel(&handle, "smoke-cancel").await {
        Ok(()) => ScenarioReport::pass(NAME, "postgres", started),
        Err(EngineError::Unavailable { op }) => {
            ScenarioReport::skip(NAME, "postgres", format!("cancel Unavailable: {op}"))
        }
        Err(EngineError::Transport { backend: be, source }) => ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("cancel transport ({be}): {source}"),
        ),
        Err(_) => ScenarioReport::pass(NAME, "postgres", started),
    }
}
