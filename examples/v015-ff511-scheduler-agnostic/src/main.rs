//! v015-ff511-scheduler-agnostic — headline demo for the v0.15.0
//! FF #511 close-out.
//!
//! Scenario: a Postgres-only deployment (no Valkey) wants to wire the
//! `ff_scheduler::Scheduler`. Pre-#511, `Scheduler::new(client, config)`
//! took a `ferriskey::Client` by value, so cairn's
//! `FabricSchedulerService::claim_for_worker` on `fabric-postgres`
//! returned `Unavailable { op: "scheduler_claim_for_worker (Postgres
//! backend has no ff-scheduler)" }`. Post-#511 the scheduler takes a
//! `Weak<dyn EngineBackend>` and an `Option<ferriskey::Client>`; PG
//! consumers use [`Scheduler::new_with_backend`] and route admission
//! primitives through the trait.
//!
//! Walkthrough:
//!   1. Connect Postgres, apply_migrations.
//!   2. Capability-gate: assert the 2 admission primitives shipped on
//!      PG (`release_admission`, `read_quota_policy_limits`) and the
//!      2 Valkey-only ones (`block_execution_for_admission`,
//!      `read_budget_usage_and_limits`) report `false` — scheduler is
//!      Valkey-only territory per RFC-023.
//!   3. `create_quota_policy` (2 req/min, concurrency cap 1).
//!   4. `read_quota_policy_limits` — the typed replacement for the
//!      pre-#511 4-HGET pattern on `ff:quota:{K}:def`.
//!   5. Construct `Scheduler::new_with_backend(Arc::downgrade(&backend), cfg)`
//!      — the FF #511 headline. No ferriskey::Client in scope.
//!   6. Call `claim_for_worker(...)` on the clientless scheduler →
//!      expect `Ok(None)` (scanner path still Valkey-bound; degrades
//!      to "no hit" instead of panicking, per the migration guide).
//!   7. Seed an admitted quota slot, call `release_admission` → verify
//!      `active_concurrency` clamps at zero.
//!   8. Idempotent replay of `release_admission` → still `Released`,
//!      counter stays at 0 (no underflow).
//!
//! ## Caveat
//!
//! For real PG worker-claim on a production deployment, use
//! `PostgresScheduler` (the PG-native claim path). This example
//! demonstrates the generic trait-only scheduler construction path
//! that FF #511 unblocks; the scanner-side primitives for
//! ZRANGEBYSCORE + exec_core HGETs are deferred to a future RFC.
//!
//! ## Prereqs
//!
//! * Postgres reachable at `FF_PG_TEST_URL`. Fresh db works; we call
//!   `apply_migrations` at boot.
//!
//! Run:
//!   FF_PG_TEST_URL=postgres://... cargo run -p v015-ff511-scheduler-agnostic-example

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::contracts::{
    CreateQuotaPolicyArgs, CreateQuotaPolicyResult, ReleaseAdmissionArgs,
    ReleaseAdmissionResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{quota_partition, PartitionConfig};
use ff_core::types::{
    ExecutionId, FlowId, LaneId, QuotaPolicyId, TimestampMs, WorkerId, WorkerInstanceId,
};
use ff_scheduler::claim::Scheduler;
use sqlx::postgres::PgPoolOptions;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "v015_ff511_scheduler_agnostic=info".into()),
        )
        .init();

    let url = std::env::var("FF_PG_TEST_URL")
        .context("FF_PG_TEST_URL not set — point at a Postgres for this demo")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .context("connect to FF_PG_TEST_URL")?;
    apply_migrations(&pool).await.context("apply_migrations")?;

    let config = PartitionConfig::default();
    let backend: Arc<PostgresBackend> = PostgresBackend::from_pool(pool.clone(), config.clone());
    let backend_dyn: Arc<dyn EngineBackend> = backend.clone();

    // ── Step 2: capability-gate ──────────────────────────────────
    let caps = backend_dyn.capabilities();
    info!(
        family = caps.identity.family,
        release_admission = caps.supports.release_admission,
        read_quota_policy_limits = caps.supports.read_quota_policy_limits,
        block_execution_for_admission = caps.supports.block_execution_for_admission,
        read_budget_usage_and_limits = caps.supports.read_budget_usage_and_limits,
        "dialed Postgres backend — FF #511 capability surface",
    );
    assert!(
        caps.supports.release_admission,
        "PG must advertise release_admission post-#511"
    );
    assert!(
        caps.supports.read_quota_policy_limits,
        "PG must advertise read_quota_policy_limits post-#511"
    );
    assert!(
        !caps.supports.block_execution_for_admission,
        "block_execution_for_admission is Valkey-only per RFC-023"
    );
    assert!(
        !caps.supports.read_budget_usage_and_limits,
        "read_budget_usage_and_limits is Valkey-only per RFC-023"
    );
    println!("== capability surface OK: PG admits the 2 shipped primitives ==");

    // ── Step 3: create_quota_policy ──────────────────────────────
    let qid = QuotaPolicyId::new();
    let create_args = CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 2,
        max_concurrent: 1,
        now: TimestampMs::now(),
    };
    match backend_dyn.create_quota_policy(create_args).await? {
        CreateQuotaPolicyResult::Created { .. } => {
            println!("create_quota_policy → Created qid={qid}")
        }
        CreateQuotaPolicyResult::AlreadySatisfied { .. } => {
            println!("create_quota_policy → AlreadySatisfied qid={qid} (idempotent replay)")
        }
    }

    // ── Step 4: read_quota_policy_limits (typed replacement for 4× HGET) ──
    let limits = backend_dyn
        .read_quota_policy_limits(&qid)
        .await?
        .context("policy just created — snapshot must be Some")?;
    println!(
        "read_quota_policy_limits → max_req_per_window={} window_s={} conc_cap={} jitter_ms={}",
        limits.max_requests_per_window,
        limits.requests_per_window_seconds,
        limits.active_concurrency_cap,
        limits.jitter_ms,
    );

    // ── Step 5: construct scheduler without a ferriskey::Client ──
    // Weak<dyn EngineBackend> breaks the cycle with any backend that
    // embeds a scheduler (ValkeyBackend does; PostgresBackend doesn't
    // but we still use Weak for uniform construction).
    let weak_backend: std::sync::Weak<dyn EngineBackend> = Arc::downgrade(&backend_dyn);
    let scheduler = Scheduler::new_with_backend(weak_backend, config.clone());
    println!("== Scheduler::new_with_backend OK — no ferriskey::Client in scope ==");

    // ── Step 6: claim_for_worker degrades to Ok(None) ────────────
    let lane = LaneId::new("v015-demo-lane");
    let worker = WorkerId::new("demo-worker");
    let worker_inst = WorkerInstanceId::new(format!("demo-worker-{}", std::process::id()));
    let caps: BTreeSet<String> = BTreeSet::new();
    match scheduler
        .claim_for_worker(&lane, &worker, &worker_inst, &caps, 5_000)
        .await?
    {
        None => println!(
            "claim_for_worker → Ok(None) — clientless scanner degrades to 'no hit' \
             (PG consumers should use PostgresScheduler for real claims)"
        ),
        Some(grant) => {
            anyhow::bail!(
                "unexpected: clientless scheduler granted a claim: {grant:?} — \
                 scanner path should have no-op'd"
            );
        }
    }

    // ── Step 7: seed an admitted slot, release it ────────────────
    let flow = FlowId::new();
    let eid = ExecutionId::for_flow(&flow, &config);
    let part = quota_partition(&qid, &config).index as i16;
    let now_ms = TimestampMs::now().0;

    sqlx::query(
        "INSERT INTO ff_quota_admitted \
             (partition_key, quota_policy_id, execution_id, admitted_at_ms, expires_at_ms) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(part)
    .bind(qid.to_string())
    .bind(eid.to_string())
    .bind(now_ms)
    .bind(now_ms + 60_000)
    .execute(&pool)
    .await
    .context("seed ff_quota_admitted")?;

    sqlx::query(
        "UPDATE ff_quota_policy SET active_concurrency = 1 \
           WHERE partition_key = $1 AND quota_policy_id = $2",
    )
    .bind(part)
    .bind(qid.to_string())
    .execute(&pool)
    .await
    .context("bump concurrency to 1")?;

    let outcome = backend_dyn
        .release_admission(ReleaseAdmissionArgs::new(qid.clone(), eid.clone()))
        .await?;
    assert_eq!(outcome, ReleaseAdmissionResult::Released);
    let conc: i64 = sqlx::query_scalar(
        "SELECT active_concurrency FROM ff_quota_policy \
           WHERE partition_key = $1 AND quota_policy_id = $2",
    )
    .bind(part)
    .bind(qid.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(conc, 0, "release_admission must decrement concurrency to 0");
    println!("release_admission → Released, active_concurrency=0");

    // ── Step 8: idempotent replay ────────────────────────────────
    let outcome2 = backend_dyn
        .release_admission(ReleaseAdmissionArgs::new(qid.clone(), eid.clone()))
        .await?;
    assert_eq!(outcome2, ReleaseAdmissionResult::Released);
    let conc2: i64 = sqlx::query_scalar(
        "SELECT active_concurrency FROM ff_quota_policy \
           WHERE partition_key = $1 AND quota_policy_id = $2",
    )
    .bind(part)
    .bind(qid.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(conc2, 0, "idempotent replay must not underflow concurrency");
    println!("release_admission (replay) → Released, active_concurrency=0 (floor clamp held)");

    // Cleanup so the demo is re-runnable against the same db.
    sqlx::query("DELETE FROM ff_quota_policy WHERE partition_key = $1 AND quota_policy_id = $2")
        .bind(part)
        .bind(qid.to_string())
        .execute(&pool)
        .await
        .ok();

    println!("== v015-ff511-scheduler-agnostic demo PASS ==");
    Ok(())
}
