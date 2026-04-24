//! Scenario dispatch + per-backend drivers.
//!
//! Each scenario module exposes two fns:
//!
//! * `run_valkey(ctx) -> ScenarioReport` — drives the HTTP path.
//! * `run_postgres(ctx, pg, backend) -> ScenarioReport` — drives the
//!   [`ff_core::EngineBackend`] trait + the Postgres backend's
//!   inherent `create_execution` directly.
//!
//! Scenarios that are currently not reachable on a backend (Postgres
//! stubs returning `EngineError::Unavailable`; flow/DAG surface not
//! yet wave-landed on Postgres; etc.) return [`ScenarioStatus::Skip`]
//! with the reason captured. In `--strict` mode the caller treats
//! Skip as Fail — a release gate must exercise every surface.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use ff_backend_postgres::{PgPool, PostgresBackend};
use ff_core::engine_backend::EngineBackend;
use ff_core::backend::BackendConfig;

use crate::{ScenarioReport, SmokeCtx};

pub mod cancel_cascade;
pub mod claim_lifecycle;
pub mod fanout_slo;
pub mod flow_anyof;
pub mod stream_best_effort;
pub mod stream_durable_summary;
pub mod suspend_signal;

/// Canonical scenario names (also used for cross-backend parity
/// comparison). Kept as a const so a name typo fails to build.
pub const NAMES: &[&str] = &[
    "claim_lifecycle",
    "flow_anyof",
    "suspend_signal",
    "stream_durable_summary",
    "stream_best_effort",
    "cancel_cascade",
    "fanout_slo",
];

/// Dial Postgres twice — once for the `Arc<dyn EngineBackend>` trait
/// surface (scenarios consume it as dyn), once for a concrete
/// `Arc<PostgresBackend>` so scenarios can reach the inherent
/// `create_execution` entry point. `Arc<dyn EngineBackend>` has no
/// `Any`-based downcast surface, so a concrete handle is the only
/// path to `create_execution` without modifying the trait.
pub async fn postgres_connect(
    url: &str,
) -> Result<(Arc<dyn EngineBackend>, Arc<PostgresBackend>, PgPool)> {
    use ff_core::backend::PostgresConnection;
    use sqlx::postgres::PgPoolOptions;

    // Trait-side handle — goes through `PostgresBackend::connect`
    // so the scenario's trait calls exercise the same codepath a
    // production ff-server build would (pool + LISTEN notifier
    // wiring included).
    let cfg_trait = BackendConfig::postgres(url);
    let trait_backend = PostgresBackend::connect(cfg_trait)
        .await
        .map_err(|e| anyhow::anyhow!("PostgresBackend::connect (trait): {e}"))?;

    // Concrete-handle path — borrow pool defaults from the connection
    // helper. `BackendConfig` is `#[non_exhaustive]` so we cannot
    // construct it via struct literal; we use its `postgres(..)` ctor
    // for the trait side above, and the raw `PgPoolOptions` here —
    // same pool defaults as `PostgresConnection::new`.
    let conn = PostgresConnection::new(url);
    let pool = PgPoolOptions::new()
        .max_connections(conn.max_connections)
        .min_connections(conn.min_connections)
        .acquire_timeout(conn.acquire_timeout)
        .connect(&conn.url)
        .await?;
    let pg = PostgresBackend::from_pool(pool.clone(), ff_core::partition::PartitionConfig::default());

    Ok((trait_backend, pg, pool))
}

pub async fn run_all_valkey(ctx: Arc<SmokeCtx>) -> Vec<ScenarioReport> {
    // Health gate: if ff-server /healthz isn't reachable, the Valkey
    // leg is unrunnable. Emit one Fail covering every scenario
    // rather than spamming seven identical errors.
    let hz = format!("{}/healthz", ctx.http_server);
    if let Err(e) = ctx.http.get(&hz).send().await.and_then(|r| r.error_for_status()) {
        let started = Instant::now();
        return NAMES
            .iter()
            .map(|name| {
                ScenarioReport::fail(
                    name,
                    "valkey",
                    started,
                    format!("ff-server /healthz unreachable: {e}"),
                )
            })
            .collect();
    }

    vec![
        claim_lifecycle::run_valkey(ctx.clone()).await,
        flow_anyof::run_valkey(ctx.clone()).await,
        suspend_signal::run_valkey(ctx.clone()).await,
        stream_durable_summary::run_valkey(ctx.clone()).await,
        stream_best_effort::run_valkey(ctx.clone()).await,
        cancel_cascade::run_valkey(ctx.clone()).await,
        fanout_slo::run_valkey(ctx.clone()).await,
    ]
}

pub async fn run_all_postgres(
    ctx: Arc<SmokeCtx>,
    handles: Option<(Arc<dyn EngineBackend>, Arc<PostgresBackend>, PgPool)>,
) -> Vec<ScenarioReport> {
    let Some((trait_backend, pg, pool)) = handles else {
        let started = Instant::now();
        return NAMES
            .iter()
            .map(|name| {
                ScenarioReport::fail(
                    name,
                    "postgres",
                    started,
                    "PostgresBackend::connect failed earlier; see log",
                )
            })
            .collect();
    };

    vec![
        claim_lifecycle::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone(), pool.clone()).await,
        flow_anyof::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone(), pool.clone()).await,
        suspend_signal::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone()).await,
        stream_durable_summary::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone()).await,
        stream_best_effort::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone()).await,
        cancel_cascade::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone()).await,
        fanout_slo::run_postgres(ctx.clone(), pg.clone(), trait_backend.clone(), pool.clone()).await,
    ]
}

// ── Shared helpers ─────────────────────────────────────────────────

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub const SMOKE_LANE: &str = "default";
pub const SMOKE_NAMESPACE: &str = "ff-smoke-v0.7";
pub const SMOKE_KIND: &str = "smoke.probe";

pub fn mint_fresh_exec_id(
    partition_config: &ff_core::partition::PartitionConfig,
) -> (ff_core::types::FlowId, ff_core::types::ExecutionId) {
    let fid = ff_core::types::FlowId::new();
    let eid = ff_core::types::ExecutionId::for_flow(&fid, partition_config);
    (fid, eid)
}

pub fn start() -> Instant {
    Instant::now()
}

/// HTTP POST /v1/executions helper used by Valkey scenarios — mirrors
/// the shape in `benches/harness/src/workload.rs` but without the
/// bench-specific deadline/header thread.
pub async fn http_create_execution(
    ctx: &SmokeCtx,
    execution_id: &ff_core::types::ExecutionId,
    payload: Vec<u8>,
) -> Result<()> {
    let partition_id = execution_id.partition();
    let body = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "namespace": SMOKE_NAMESPACE,
        "lane_id": SMOKE_LANE,
        "execution_kind": SMOKE_KIND,
        "input_payload": payload,
        "payload_encoding": "binary",
        "priority": 100,
        "creator_identity": "ff-smoke-v0.7",
        "tags": {},
        "partition_id": partition_id,
        "now": now_ms(),
    });
    let url = format!("{}/v1/executions", ctx.http_server);
    let resp = ctx.http.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("create_execution failed ({status}): {text}");
    }
    Ok(())
}

/// Poll /v1/executions/{id}/state until a terminal value appears or
/// the deadline elapses. Returns the terminal state. Kept for
/// scenarios that later want terminal assertion (today every
/// scenario is satisfied by visibility); `#[allow(dead_code)]` so
/// the unused helper doesn't block clippy -D warnings in CI.
#[allow(dead_code)]
pub async fn http_wait_terminal(
    ctx: &SmokeCtx,
    eid: &ff_core::types::ExecutionId,
    deadline: std::time::Duration,
) -> Result<String> {
    let url = format!("{}/v1/executions/{}/state", ctx.http_server, eid);
    let start = Instant::now();
    loop {
        let resp = ctx.http.get(&url).send().await?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await?;
            let s = v.as_str().unwrap_or("unknown").to_owned();
            if matches!(s.as_str(), "completed" | "failed" | "cancelled" | "expired") {
                return Ok(s);
            }
        }
        if start.elapsed() > deadline {
            anyhow::bail!("wait_terminal {eid}: deadline exceeded (state never terminal)");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Build a `CreateExecutionArgs` appropriate for the Postgres inherent
/// `create_execution` path. Keeps the shape minimal — smoke doesn't
/// need to exercise every optional field.
pub fn build_create_args(
    partition_config: &ff_core::partition::PartitionConfig,
) -> (ff_core::types::ExecutionId, ff_core::contracts::CreateExecutionArgs) {
    let (_fid, eid) = mint_fresh_exec_id(partition_config);
    let partition_id = eid.partition();
    let args = ff_core::contracts::CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: ff_core::types::Namespace::new(SMOKE_NAMESPACE),
        lane_id: ff_core::types::LaneId::new(SMOKE_LANE),
        execution_kind: SMOKE_KIND.to_string(),
        input_payload: Vec::new(),
        payload_encoding: Some("binary".to_string()),
        priority: 100,
        creator_identity: "ff-smoke-v0.7".to_string(),
        idempotency_key: None,
        tags: std::collections::HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id,
        now: ff_core::types::TimestampMs::from_millis(now_ms()),
    };
    (eid, args)
}

/// Claim one execution through the EngineBackend trait. Returns
/// `Ok(Some(handle))` on success, `Ok(None)` if no work is claimable
/// (which for the smoke is a Fail — we just seeded one), or an error.
pub async fn claim_one(
    backend: &Arc<dyn EngineBackend>,
) -> std::result::Result<Option<ff_core::backend::Handle>, ff_core::engine_error::EngineError> {
    use ff_core::backend::{CapabilitySet, ClaimPolicy};
    use ff_core::types::{LaneId, WorkerId, WorkerInstanceId};
    let lane = LaneId::new(SMOKE_LANE);
    let caps = CapabilitySet::new(Vec::<String>::new());
    let policy = ClaimPolicy::new(
        WorkerId::new(format!("smoke-{}", uuid::Uuid::new_v4())),
        WorkerInstanceId::new(format!("smoke-inst-{}", uuid::Uuid::new_v4())),
        30_000,
        None,
    );
    backend.claim(&lane, &caps, policy).await
}
