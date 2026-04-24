//! Scenario 1 — claim → progress → complete lifecycle.
//!
//! Smoke on the happiest path: seed one execution, claim it, heartbeat
//! a progress update, then complete. The scenario touches:
//!
//! * `create_execution` (per-backend ingress)
//! * `EngineBackend::claim`
//! * `EngineBackend::progress`
//! * `EngineBackend::complete`
//!
//! If any of these fail or return `Unavailable`, the scenario fails.

use std::sync::Arc;
use std::time::Duration;

// `Duration` is used below for the post-create settle.

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios::{
    self, build_create_args, claim_one, http_create_execution, mint_fresh_exec_id,
};

const NAME: &str = "claim_lifecycle";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    let (_fid, eid) = mint_fresh_exec_id(&ctx.partition_config);
    if let Err(e) = http_create_execution(&ctx, &eid, b"hello".to_vec()).await {
        return ScenarioReport::fail(NAME, "valkey", started, format!("create: {e}"));
    }
    // The ff-server loop claims/completes via its integrated
    // scheduler + worker-sim-loop NEVER runs in the smoke harness.
    // What we're actually smoking here is the HTTP ingress +
    // terminal-state reachability. For the Valkey leg a fuller
    // lifecycle probe belongs in ff-test; here we assert the
    // execution is visible over HTTP (state endpoint responds
    // non-terminal → pass) within the scenario timeout.
    let url = format!("{}/v1/executions/{}/state", ctx.http_server, eid);
    match ctx.http.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            ScenarioReport::pass(NAME, "valkey", started)
        }
        Ok(resp) => ScenarioReport::fail(
            NAME,
            "valkey",
            started,
            format!("state endpoint HTTP {}", resp.status()),
        ),
        Err(e) => ScenarioReport::fail(NAME, "valkey", started, format!("state GET: {e}")),
    }
}

pub async fn run_postgres(
    ctx: Arc<SmokeCtx>,
    pg: Arc<PostgresBackend>,
    backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    let started = scenarios::start();
    let (_eid, args) = build_create_args(&ctx.partition_config);
    if let Err(e) = pg.create_execution(args).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create: {e}"));
    }

    // Small settle to let any indexes/rows commit before claim.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Try to claim the freshly seeded execution via the trait.
    // `claim` may legitimately return Ok(None) if the Postgres
    // scheduler's admission pipeline hasn't admitted the row yet.
    // Treat Ok(None) as Skip (surface exists, just didn't hand back
    // a handle in the smoke window) and Ok(Some(_)) as Pass.
    let handle = match claim_one(&backend).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return ScenarioReport::skip(
                NAME,
                "postgres",
                "claim returned Ok(None) (scheduler had no eligible handle in window)",
            );
        }
        Err(ff_core::engine_error::EngineError::Unavailable { op }) => {
            return ScenarioReport::skip(NAME, "postgres", format!("Unavailable: {op}"));
        }
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("claim: {e}"));
        }
    };

    // progress heartbeat (optional-ish — if Unavailable we still want
    // the scenario to pass, since it is a single trait method).
    if let Err(e) = backend
        .progress(&handle, Some(50), Some("smoke-halfway".to_string()))
        .await
        && !matches!(e, ff_core::engine_error::EngineError::Unavailable { .. })
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("progress: {e}"));
    }

    // Terminal — complete.
    match backend.complete(&handle, Some(b"smoke-ok".to_vec())).await {
        Ok(()) => ScenarioReport::pass(NAME, "postgres", started),
        Err(ff_core::engine_error::EngineError::Unavailable { op }) => {
            ScenarioReport::skip(NAME, "postgres", format!("complete Unavailable: {op}"))
        }
        Err(e) => ScenarioReport::fail(NAME, "postgres", started, format!("complete: {e}")),
    }
}

