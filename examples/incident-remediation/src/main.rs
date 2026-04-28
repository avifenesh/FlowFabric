//! incident-remediation — SC-10 headline example for the RFC-024
//! lease-reclaim consumer surface (v0.12.0).
//!
//! # Scenario
//!
//! A pager fires. Responder A claims the incident execution, works
//! through a multi-step remediation (simulated:
//! `diagnose → apply_fix → verify`), and becomes unresponsive
//! mid-flow (pager-death: drops without calling `complete`). A
//! supervisor issues a [`ReclaimGrant`] via the backend's RFC-024
//! admission path. Responder B picks up via
//! [`FlowFabricWorker::claim_from_reclaim_grant`] and completes the
//! remediation. After running past `max_reclaim_count` the backend
//! returns [`ReclaimExecutionOutcome::ReclaimCapExceeded`] — the
//! "we've reclaimed too many times, escalate to a human" signal.
//!
//! # Run
//!
//! ```text
//! # SQLite (zero-infra):
//! FF_DEV_MODE=1 cargo run -p incident-remediation -- --backend sqlite
//!
//! # Valkey / Postgres:
//! cargo run -p incident-remediation -- --backend valkey
//! cargo run -p incident-remediation -- --backend postgres
//! ```
//!
//! See the example README for the setup matrix.
//!
//! # Migration reference
//!
//! `docs/CONSUMER_MIGRATION_0.12.md` §7 documents the production
//! cairn-fabric migration pattern this scenario dramatises.
//!
//! [`ReclaimGrant`]: ff_core::contracts::ReclaimGrant
//! [`FlowFabricWorker::claim_from_reclaim_grant`]: ff_sdk::FlowFabricWorker::claim_from_reclaim_grant
//! [`ReclaimExecutionOutcome::ReclaimCapExceeded`]: ff_core::contracts::ReclaimExecutionOutcome

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ff_core::backend::{BackendConfig, CapabilitySet, ClaimPolicy};
use ff_core::contracts::{
    AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs, IssueReclaimGrantArgs,
    IssueReclaimGrantOutcome, ReclaimExecutionArgs, ReclaimExecutionOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, FlowId, LaneId, LeaseId, Namespace, TimestampMs,
    WorkerId, WorkerInstanceId,
};
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use tracing::info;
use uuid::Uuid;

/// `max_reclaim_count` for the demo. Set low so the cap-exceeded
/// branch triggers in a handful of iterations. Production default is
/// 1000 (RFC-024 §4.6) — raise this in real deployments; `2` is the
/// "developer can watch it fire end-to-end" value.
const DEMO_MAX_RECLAIM_COUNT: u32 = 2;

/// Per-responder lease TTL. Not load-bearing for the scenario — we
/// simulate lease expiry via direct column update rather than waiting
/// the TTL out, so the scanner-driven expiry path is replayed
/// deterministically.
const LEASE_TTL_MS: u64 = 30_000;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Backend {
    Sqlite,
    Postgres,
    Valkey,
}

#[derive(Parser, Debug)]
#[command(about = "SC-10 incident-remediation — RFC-024 reclaim demo")]
struct Args {
    /// Which backend to run the scenario against.
    #[arg(long, value_enum, default_value_t = Backend::Valkey)]
    backend: Backend,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Match all span targets — the example emits under
                // `engine`/`supervisor`/`responder-a`/`responder-b`
                // (via `info!(target: "…", …)`), not under the crate
                // path — so a crate-scoped filter would swallow them.
                .unwrap_or_else(|_| "info,ff_backend_sqlite=warn".into()),
        )
        .init();

    let args = Args::parse();
    info!(target: "engine", backend = ?args.backend, "[engine] SC-10 incident-remediation starting");

    match args.backend {
        Backend::Sqlite => run_sqlite().await,
        Backend::Postgres => {
            let url = std::env::var("FF_POSTGRES_URL")
                .context("FF_POSTGRES_URL must be set for --backend postgres")?;
            let _backend = ff_backend_postgres::PostgresBackend::connect(
                BackendConfig::postgres(url),
            )
            .await
            .context("PostgresBackend::connect")?;
            info!(target: "engine", "[engine] connected to Postgres");
            // PG/Valkey need a live scheduler + scanner supervisor to drive
            // lease-expiry transitions. The self-contained SQLite path is
            // the zero-infra showcase; for PG/Valkey we document the
            // required operator setup in the README and stop here.
            anyhow::bail!(
                "PG path needs a live scheduler+scanner deployment; see README.\n\
                 For end-to-end without external services, re-run with\n\
                 `FF_DEV_MODE=1 cargo run -p incident-remediation -- --backend sqlite`."
            );
        }
        Backend::Valkey => {
            let backend = ff_backend_valkey::ValkeyBackend::connect(BackendConfig::valkey(
                "127.0.0.1",
                6379,
            ))
            .await
            .context("ValkeyBackend::connect")?;
            info!(target: "engine", "[engine] connected to Valkey");
            let _ = backend;
            anyhow::bail!(
                "Valkey path needs a live scheduler+scanner deployment; see README.\n\
                 For end-to-end without external services, re-run with\n\
                 `FF_DEV_MODE=1 cargo run -p incident-remediation -- --backend sqlite`."
            );
        }
    }
}

// ────────────────────────────── SQLite end-to-end ──────────────────────────────

async fn run_sqlite() -> Result<()> {
    if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
        anyhow::bail!(
            "FF_DEV_MODE=1 is required for --backend sqlite. Re-run with\n\
             `FF_DEV_MODE=1 cargo run -p incident-remediation -- --backend sqlite`."
        );
    }

    let db_uri = format!("file:sc10-{}?mode=memory&cache=shared", Uuid::new_v4());
    let backend = ff_backend_sqlite::SqliteBackend::new(&db_uri)
        .await
        .context("construct SqliteBackend")?;
    let trait_obj: Arc<dyn EngineBackend> = backend.clone();
    info!(target: "engine", "[engine] SQLite dev backend ready");

    // Side-pool mirrors examples/ff-dev — dev-only column surgery the
    // Valkey/PG scheduler would drive in production.
    let side_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            db_uri
                .parse::<sqlx::sqlite::SqliteConnectOptions>()
                .context("parse dev URI for side-pool")?,
        )
        .await
        .context("open side-pool")?;

    // ── 1. Supervisor provisions the incident execution ─────────────────
    let namespace = Namespace::new("incidents");
    let lane = LaneId::new("pagerduty");
    let flow_id = FlowId::from_uuid(Uuid::new_v4());
    let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id");
    let now = TimestampMs::from_millis(1_000);

    trait_obj
        .create_flow(CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "incident".into(),
            namespace: namespace.clone(),
            now,
        })
        .await?;
    trait_obj
        .create_execution(CreateExecutionArgs {
            execution_id: exec_id.clone(),
            namespace: namespace.clone(),
            lane_id: lane.clone(),
            execution_kind: "remediate".into(),
            input_payload: b"INC-4711: db-primary disk 97%".to_vec(),
            payload_encoding: None,
            priority: 10_000,
            creator_identity: "supervisor".into(),
            idempotency_key: None,
            tags: Default::default(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: 0,
            now,
        })
        .await?;
    trait_obj
        .add_execution_to_flow(AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: exec_id.clone(),
            now,
        })
        .await?;
    promote_to_runnable(&side_pool, &exec_id).await?;
    info!(target: "supervisor", execution = %exec_id, "[supervisor] incident filed — execution runnable");

    // ── 2. Build two worker SDK clients (Responder A, Responder B) ──────
    let worker_a = build_worker(
        trait_obj.clone(),
        "responder-a",
        "responder-a-instance-1",
        &lane,
    )
    .await?;
    let worker_b = build_worker(
        trait_obj.clone(),
        "responder-b",
        "responder-b-instance-1",
        &lane,
    )
    .await?;

    // ── 3. Responder A claims + starts working ──────────────────────────
    let claim_policy_a = ClaimPolicy::new(
        WorkerId::new("responder-a"),
        WorkerInstanceId::new("responder-a-instance-1"),
        LEASE_TTL_MS as u32,
        None,
    );
    let handle_a = trait_obj
        .claim(&lane, &CapabilitySet::default(), claim_policy_a)
        .await?
        .context("Responder A: claim returned None")?;
    info!(target: "responder-a", execution = %exec_id, "[responder-a] pager fired — claimed incident");
    info!(target: "responder-a", "[responder-a] step 1/3: diagnose — db-primary disk check");
    info!(target: "responder-a", "[responder-a] step 2/3: apply_fix — rotating log volumes …");
    // ── pager-death simulated here — Responder A drops without calling `complete`.
    info!(target: "responder-a", "[responder-a] (pager-death — process vanishes mid-flow)");
    let _ = handle_a; // handle is abandoned intentionally

    // ── 4. Scanner/TTL would flip ownership_state; simulate it ──────────
    force_lease_expired(&side_pool, &exec_id).await?;

    // ── 5. Supervisor issues a reclaim grant (RFC-024 §3.2) ─────────────
    //
    // NB: in a deployment with `ff-server` reachable, this is
    // `FlowFabricAdminClient::issue_reclaim_grant` over HTTP. Under
    // FF_DEV_MODE=1 + SQLite there is no ff-server, so we invoke the
    // trait method directly — same primitive, no transport round-trip.
    // See `docs/CONSUMER_MIGRATION_0.12.md` §7 for the HTTP shape.
    let grant = issue_grant(
        trait_obj.as_ref(),
        &exec_id,
        &lane,
        "responder-b",
        "responder-b-instance-1",
    )
    .await?;
    info!(target: "supervisor", execution = %exec_id, grant = %grant.grant_key, "[supervisor] issued reclaim grant for Responder B");

    // ── 6. Responder B consumes the grant via the SDK ───────────────────
    let outcome = worker_b
        .claim_from_reclaim_grant(
            grant,
            reclaim_args(
                &exec_id,
                &lane,
                "responder-b",
                "responder-b-instance-1",
                "responder-a-instance-1",
                0, // current_attempt_index before reclaim
                Some(DEMO_MAX_RECLAIM_COUNT),
            ),
        )
        .await?;
    let handle_b = match outcome {
        ReclaimExecutionOutcome::Claimed(h) => h,
        other => anyhow::bail!("expected Claimed, got {other:?}"),
    };
    info!(target: "responder-b", execution = %exec_id, "[responder-b] claimed via reclaim grant (HandleKind::Reclaimed)");
    info!(target: "responder-b", "[responder-b] step 3/3: verify — alert resolved, disk at 45%");
    trait_obj
        .complete(&handle_b, Some(b"INC-4711 resolved".to_vec()))
        .await?;
    info!(target: "responder-b", "[responder-b] complete — incident closed");

    // ── 7. Demonstrate `ReclaimCapExceeded` — the escalation branch ─────
    //
    // Provision a SECOND incident, run it past `max_reclaim_count`
    // reclaims, and assert the cap-exceeded outcome lands. Uses the
    // same worker + grant path as above, looped.
    let escalated = ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id");
    trait_obj
        .create_execution(CreateExecutionArgs {
            execution_id: escalated.clone(),
            namespace: namespace.clone(),
            lane_id: lane.clone(),
            execution_kind: "remediate".into(),
            input_payload: b"INC-4712: flapping service".to_vec(),
            payload_encoding: None,
            priority: 10_000,
            creator_identity: "supervisor".into(),
            idempotency_key: None,
            tags: Default::default(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: 0,
            now,
        })
        .await?;
    promote_to_runnable(&side_pool, &escalated).await?;
    info!(target: "supervisor", execution = %escalated, "[supervisor] second incident filed — running past max_reclaim_count={DEMO_MAX_RECLAIM_COUNT}");

    let _ = worker_a; // keep the worker alive for symmetry
    // Initial claim (attempt_index=0) that we'll immediately abandon.
    let claim_policy = ClaimPolicy::new(
        WorkerId::new("responder-a"),
        WorkerInstanceId::new("responder-a-instance-1"),
        LEASE_TTL_MS as u32,
        None,
    );
    let _ = trait_obj
        .claim(&lane, &CapabilitySet::default(), claim_policy)
        .await?
        .context("claim attempt 0")?;

    for round in 0..=(DEMO_MAX_RECLAIM_COUNT as usize) {
        info!(target: "engine", round, "[engine] simulating lease expiry + reclaim round");
        force_lease_expired(&side_pool, &escalated).await?;
        let grant = issue_grant(
            trait_obj.as_ref(),
            &escalated,
            &lane,
            "responder-b",
            "responder-b-instance-1",
        )
        .await?;
        let args = reclaim_args(
            &escalated,
            &lane,
            "responder-b",
            "responder-b-instance-1",
            "responder-a-instance-1",
            round as u32,
            Some(DEMO_MAX_RECLAIM_COUNT),
        );
        let outcome = worker_b.claim_from_reclaim_grant(grant, args).await?;
        match outcome {
            ReclaimExecutionOutcome::Claimed(_) => {
                info!(target: "responder-b", round, "[responder-b] reclaimed (pager-death simulated again)");
            }
            ReclaimExecutionOutcome::ReclaimCapExceeded { reclaim_count, .. } => {
                info!(
                    target: "supervisor",
                    reclaim_count,
                    cap = DEMO_MAX_RECLAIM_COUNT,
                    "[supervisor] ReclaimCapExceeded — paging a human, execution moved to terminal_failed"
                );
                info!(target: "engine", "[engine] SC-10 scenario complete — reclaim happy-path + cap-exceeded both exercised");
                return Ok(());
            }
            other => anyhow::bail!("unexpected reclaim outcome at round {round}: {other:?}"),
        }
    }
    anyhow::bail!(
        "ran {} rounds without hitting ReclaimCapExceeded — check DEMO_MAX_RECLAIM_COUNT wiring",
        DEMO_MAX_RECLAIM_COUNT
    )
}

// ────────────────────────────── helpers ──────────────────────────────

async fn build_worker(
    backend: Arc<dyn EngineBackend>,
    worker_id: &str,
    worker_instance_id: &str,
    lane: &LaneId,
) -> Result<FlowFabricWorker> {
    // `BackendConfig` is only consulted by `FlowFabricWorker::connect`
    // (the Valkey-native entry point); `connect_with` ignores it and
    // relies entirely on the injected `Arc<dyn EngineBackend>`.
    let config = WorkerConfig {
        backend: BackendConfig::valkey("127.0.0.1", 6379),
        worker_id: WorkerId::new(worker_id),
        worker_instance_id: WorkerInstanceId::new(worker_instance_id),
        namespace: Namespace::new("incidents"),
        lanes: vec![lane.clone()],
        capabilities: vec![],
        lease_ttl_ms: LEASE_TTL_MS,
        claim_poll_interval_ms: 500,
        max_concurrent_tasks: 4,
        partition_config: None,
    };
    FlowFabricWorker::connect_with(config, backend, None)
        .await
        .context("FlowFabricWorker::connect_with")
}

async fn issue_grant(
    backend: &dyn EngineBackend,
    exec_id: &ExecutionId,
    lane: &LaneId,
    worker_id: &str,
    worker_instance_id: &str,
) -> Result<ff_core::contracts::ReclaimGrant> {
    let outcome = backend
        .issue_reclaim_grant(IssueReclaimGrantArgs::new(
            exec_id.clone(),
            WorkerId::new(worker_id),
            WorkerInstanceId::new(worker_instance_id),
            lane.clone(),
            None,
            60_000,
            None,
            None,
            Default::default(),
            TimestampMs::from_millis(now_ms()),
        ))
        .await
        .context("issue_reclaim_grant")?;
    match outcome {
        IssueReclaimGrantOutcome::Granted(g) => Ok(g),
        other => anyhow::bail!("expected Granted, got {other:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn reclaim_args(
    exec_id: &ExecutionId,
    lane: &LaneId,
    worker_id: &str,
    worker_instance_id: &str,
    old_worker_instance_id: &str,
    current_attempt_index: u32,
    max_reclaim_count: Option<u32>,
) -> ReclaimExecutionArgs {
    ReclaimExecutionArgs::new(
        exec_id.clone(),
        WorkerId::new(worker_id),
        WorkerInstanceId::new(worker_instance_id),
        lane.clone(),
        None,
        LeaseId::new(),
        LEASE_TTL_MS,
        AttemptId::new(),
        String::new(),
        max_reclaim_count,
        WorkerInstanceId::new(old_worker_instance_id),
        AttemptIndex::new(current_attempt_index),
    )
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn uuid_of(eid: &ExecutionId) -> Uuid {
    Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
}

/// Promote a freshly-created execution into the `runnable` / `pending`
/// / `initial` shape the claim path expects. Dev-only analogue of the
/// scheduler+scanner promotion pass. Mirrors `examples/ff-dev`.
async fn promote_to_runnable(pool: &sqlx::SqlitePool, exec_id: &ExecutionId) -> Result<()> {
    sqlx::query(
        "UPDATE ff_exec_core \
            SET lifecycle_phase='runnable', public_state='pending', attempt_state='initial' \
          WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(uuid_of(exec_id))
    .execute(pool)
    .await
    .context("UPDATE ff_exec_core (promote)")?;
    Ok(())
}

/// Force `ownership_state = 'lease_expired_reclaimable'` on an
/// already-claimed execution — dev-only stand-in for the scanner's
/// TTL-driven transition. Mirrors `ff-backend-sqlite` RFC-024 tests.
async fn force_lease_expired(pool: &sqlx::SqlitePool, exec_id: &ExecutionId) -> Result<()> {
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='lease_expired_reclaimable' \
          WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(uuid_of(exec_id))
    .execute(pool)
    .await
    .context("UPDATE ff_exec_core (lease_expired)")?;
    Ok(())
}
