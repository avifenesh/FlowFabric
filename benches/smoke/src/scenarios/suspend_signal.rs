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
//! require a 4xx (5xx indicates a real wiring bug). Postgres leg
//! (Wave 7b unskip, post-Wave-4d): seed an execution, claim it via
//! the trait, rotate a kid into `ff_waitpoint_hmac`, `suspend` with
//! a single-waitpoint `ResumeCondition::Single`, then
//! `deliver_signal` and assert the `resume_condition_satisfied`
//! effect string.

use std::sync::Arc;
use std::time::Duration;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    DeliverSignalArgs, DeliverSignalResult, ResumeCondition, ResumePolicy,
    RotateWaitpointHmacSecretAllArgs, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::{SignalId, SuspensionId, TimestampMs, WaitpointId};

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
    ctx: Arc<SmokeCtx>,
    pg: Arc<PostgresBackend>,
    backend: Arc<dyn EngineBackend>,
) -> ScenarioReport {
    let started = scenarios::start();

    // 1. Seed + claim an execution.
    //
    // `create_execution` lands the row at `lifecycle_phase='submitted'`
    // and the Pg admission pipeline that promotes `submitted` →
    // `runnable` isn't wave-landed yet (Wave 4b only shipped the
    // claim trait surface, not the admission scanner). For the smoke
    // we promote by hand via the same `FF_SMOKE_POSTGRES_URL` the
    // harness already dials — keeps the scenario self-contained
    // without forcing an admission-reconciler port into this wave.
    let (eid, args) = scenarios::build_create_args(&ctx.partition_config);
    if let Err(e) = pg.create_execution(args).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create: {e}"));
    }

    let pg_url = std::env::var("FF_SMOKE_POSTGRES_URL").unwrap_or_else(|_| {
        "postgres://postgres:postgres@localhost:5432/ff_smoke".to_owned()
    });
    let side_pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&pg_url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("side pool: {e}"));
        }
    };
    let exec_uuid = match uuid::Uuid::parse_str(
        eid.as_str().split_once("}:").map(|(_, s)| s).unwrap_or(""),
    ) {
        Ok(u) => u,
        Err(e) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!("parse exec uuid: {e}"),
            );
        }
    };
    if let Err(e) = sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'runnable' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(eid.partition() as i16)
    .bind(exec_uuid)
    .execute(&side_pool)
    .await
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("promote: {e}"));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Rotate a kid up-front — scheduler.claim_for_worker fails closed
    // if `ff_waitpoint_hmac` has no active row. Replay on same
    // (kid, secret) is Noop, so safe across scenario runs.
    let rotate_args = RotateWaitpointHmacSecretAllArgs::new(
        "smoke-kid-v1",
        "ab".repeat(32),
        0,
    );
    if let Err(e) = backend.rotate_waitpoint_hmac_secret_all(rotate_args).await {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("rotate_waitpoint_hmac: {e}"),
        );
    }

    let handle = match scenarios::claim_one(&backend).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return ScenarioReport::skip(
                NAME,
                "postgres",
                "claim returned Ok(None) (scheduler had no eligible handle in window)",
            );
        }
        Err(EngineError::Unavailable { op }) => {
            return ScenarioReport::skip(NAME, "postgres", format!("claim Unavailable: {op}"));
        }
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("claim: {e}"));
        }
    };

    // 2. Suspend on a Single{ByName("ready")} condition. Using ByName
    //    (not Wildcard) mirrors the ff-test fixture exactly — ByName
    //    is the common-case matcher and gives the scenario a precise
    //    resume signal to deliver.
    let wp_id = WaitpointId::new();
    let wp_key = format!("wpk:smoke:{wp_id}");
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: wp_key.clone(),
    };
    let cond = ResumeCondition::Single {
        waitpoint_key: wp_key.clone(),
        matcher: SignalMatcher::ByName("ready".into()),
    };
    let suspend_args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let outcome = match backend.suspend(&handle, suspend_args).await {
        Ok(o) => o,
        Err(EngineError::Unavailable { op }) => {
            return ScenarioReport::skip(NAME, "postgres", format!("suspend Unavailable: {op}"));
        }
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("suspend: {e}"));
        }
    };
    let SuspendOutcome::Suspended { details, .. } = outcome else {
        return ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            "suspend returned AlreadySatisfied on a fresh waitpoint".to_string(),
        );
    };
    let token = details.waitpoint_token.token().clone();

    // 3. Deliver a matching signal; expect resume_condition_satisfied.
    let ds = DeliverSignalArgs {
        execution_id: eid.clone(),
        waitpoint_id: wp_id,
        signal_id: SignalId::new(),
        signal_name: "ready".into(),
        signal_category: "external".into(),
        source_type: "worker".into(),
        source_identity: "ff-smoke-v0.7".into(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: None,
        target_scope: "execution".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: token,
        now: TimestampMs::now(),
    };
    let result = match backend.deliver_signal(ds).await {
        Ok(r) => r,
        Err(EngineError::Unavailable { op }) => {
            return ScenarioReport::skip(
                NAME,
                "postgres",
                format!("deliver_signal Unavailable: {op}"),
            );
        }
        Err(e) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!("deliver_signal: {e}"),
            );
        }
    };
    match result {
        DeliverSignalResult::Accepted { effect, .. }
            if effect == "resume_condition_satisfied" =>
        {
            ScenarioReport::pass(NAME, "postgres", started)
        }
        DeliverSignalResult::Accepted { effect, .. } => ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            format!("unexpected effect: {effect}"),
        ),
        DeliverSignalResult::Duplicate { .. } => ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            "unexpected Duplicate on first delivery".to_string(),
        ),
    }
}
