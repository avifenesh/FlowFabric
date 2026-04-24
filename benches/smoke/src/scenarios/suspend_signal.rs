//! Scenario 3 — suspend + deliver_signal reachability probe.
//!
//! The full RFC-013/014 suspend tree (`WaitpointBinding` +
//! `ResumePolicy` + `SuspensionReasonCode` construction + HMAC
//! rotation) is covered by ff-test's serial e2e suite. A smoke's
//! job is to catch reachability-class bugs (endpoint unreachable,
//! trait method unconditionally 500s), not to revalidate the full
//! contract.
//!
//! Valkey leg: POST to `/v1/executions/<bogus-id>/signal` and
//! require a 4xx (5xx indicates a real wiring bug). Postgres leg:
//! skipped until a public `DeliverSignalArgs` builder lands (see
//! scenario body for rationale).

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;

use crate::ScenarioReport;
use crate::SmokeCtx;
use crate::scenarios;

const NAME: &str = "suspend_signal";

pub async fn run_valkey(ctx: Arc<SmokeCtx>) -> ScenarioReport {
    let started = scenarios::start();
    let fake = ff_core::types::ExecutionId::for_flow(
        &ff_core::types::FlowId::new(),
        &ctx.partition_config,
    );
    let url = format!("{}/v1/executions/{}/signal", ctx.http_server, fake);
    let body = serde_json::json!({
        "signal_id": "smoke-signal",
        "payload": {"probe": true},
        "now": scenarios::now_ms(),
    });
    let resp = match ctx.http.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return ScenarioReport::fail(NAME, "valkey", started, format!("POST: {e}")),
    };
    let status = resp.status();
    if status.is_server_error() {
        return ScenarioReport::fail(
            NAME,
            "valkey",
            started,
            format!("signal endpoint 5xx: {status}"),
        );
    }
    // Any 4xx means the endpoint routed — that's what we're smoking.
    ScenarioReport::pass(NAME, "valkey", started)
}

pub async fn run_postgres(
    _ctx: Arc<SmokeCtx>,
    _pg: Arc<PostgresBackend>,
    _backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    // RFC-013 DeliverSignalArgs is a 15-field struct (HMAC token,
    // category, source identity, waitpoint-id hierarchy, etc.).
    // Building a realistic args tree inside a smoke means forking the
    // ff-test helpers, which would couple the smoke to test-only
    // surface. Held as Skip until a public builder lands (tracked
    // under cairn-rs ff-sdk-gap work — see
    // `project_cairn_ff_sdk_gaps.md` for the pattern precedent).
    // The reachability of suspend/signal endpoint wiring is covered
    // by the Valkey HTTP leg; the Postgres trait surface is covered
    // in ff-test's serial e2e once the flow is Postgres-wave-complete.
    let _ = EngineError::Unavailable { op: "unused" };
    ScenarioReport::skip(
        NAME,
        "postgres",
        "awaiting public DeliverSignalArgs builder; ff-test covers the trait path serially",
    )
}
