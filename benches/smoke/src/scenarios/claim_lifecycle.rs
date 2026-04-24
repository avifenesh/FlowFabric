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
//! On Postgres we also exercise [`PostgresScheduler::claim_for_worker`]
//! (Wave 5b PR #246) on a separate seeded execution so the signed-grant
//! issuance path is reachability-probed too.
//!
//! The submit→runnable promoter is deliberately out-of-scope on Postgres
//! (see Wave 5b docs + `pg_scheduler_claim.rs::promote_to_runnable`), so
//! the smoke replicates the documented test pattern: manual UPDATE to
//! `lifecycle_phase='runnable'` after create_execution.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use ff_backend_postgres::{PgPool, PostgresBackend};
use ff_backend_postgres::scheduler::PostgresScheduler;
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{WorkerId, WorkerInstanceId};

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
    pool: PgPool,
) -> ScenarioReport {
    let started = scenarios::start();

    // ── Exec A: exercise the scheduler.claim_for_worker grant-issuance
    //           path (Wave 5b PR #246).
    let (eid_a, args_a) = build_create_args(&ctx.partition_config);
    if let Err(e) = pg.create_execution(args_a).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create A: {e}"));
    }
    if let Err(e) = promote_to_runnable(&pool, &eid_a).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("promote A: {e}"));
    }
    // Seed a single HMAC secret so the scheduler can sign. The
    // v0.7 release gate script exports FF_WAITPOINT_HMAC_SECRET;
    // ensure a row exists in `ff_waitpoint_hmac` regardless so the
    // smoke is self-sufficient. Upsert-by-hash keeps concurrent
    // scenarios from duplicating the row.
    if let Err(e) = ensure_hmac_secret(&pool).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("hmac seed: {e}"));
    }

    let scheduler = PostgresScheduler::new(pool.clone());
    let lane = ff_core::types::LaneId::new(scenarios::SMOKE_LANE);
    let caps: BTreeSet<String> = BTreeSet::new();
    let wid = WorkerId::new(format!("smoke-{}", uuid::Uuid::new_v4()));
    let wiid = WorkerInstanceId::new(format!("smoke-inst-{}", uuid::Uuid::new_v4()));
    match scheduler
        .claim_for_worker(&lane, &wid, &wiid, &caps, 30_000)
        .await
    {
        Ok(Some(_grant)) => {}
        Ok(None) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                "scheduler.claim_for_worker returned None for a freshly-runnable row",
            );
        }
        Err(e) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                format!("scheduler.claim_for_worker: {e}"),
            );
        }
    }

    // ── Exec B: exercise the claim → progress → complete lifecycle
    //           through the `EngineBackend` trait.
    let (eid_b, args_b) = build_create_args(&ctx.partition_config);
    if let Err(e) = pg.create_execution(args_b).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("create B: {e}"));
    }
    if let Err(e) = promote_to_runnable(&pool, &eid_b).await {
        return ScenarioReport::fail(NAME, "postgres", started, format!("promote B: {e}"));
    }
    // Small settle to let any indexes/rows commit before claim.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let handle = match claim_one(&backend).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return ScenarioReport::fail(
                NAME,
                "postgres",
                started,
                "backend.claim returned None for a freshly-runnable row",
            );
        }
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("claim: {e}"));
        }
    };

    if let Err(e) = backend
        .progress(&handle, Some(50), Some("smoke-halfway".to_string()))
        .await
        && !matches!(e, ff_core::engine_error::EngineError::Unavailable { .. })
    {
        return ScenarioReport::fail(NAME, "postgres", started, format!("progress: {e}"));
    }

    match backend.complete(&handle, Some(b"smoke-ok".to_vec())).await {
        Ok(()) => {}
        Err(e) => {
            return ScenarioReport::fail(NAME, "postgres", started, format!("complete: {e}"));
        }
    }

    // Verify terminal state committed.
    match verify_terminal(&pool, &eid_b).await {
        Ok(true) => ScenarioReport::pass(NAME, "postgres", started),
        Ok(false) => ScenarioReport::fail(
            NAME,
            "postgres",
            started,
            "exec B did not reach lifecycle_phase='terminal' after complete",
        ),
        Err(e) => ScenarioReport::fail(NAME, "postgres", started, format!("verify: {e}")),
    }
}

/// Flip a freshly-created execution from `submitted` → `runnable` so
/// the `claim` / scheduler pipelines can pick it up. Mirrors the
/// documented test pattern in `crates/ff-test/tests/pg_scheduler_claim.rs`.
async fn promote_to_runnable(
    pool: &PgPool,
    eid: &ff_core::types::ExecutionId,
) -> Result<(), sqlx::Error> {
    let part_i16 = eid.partition() as i16;
    let uuid = parse_exec_uuid(eid);
    sqlx::query(
        "UPDATE ff_exec_core \
            SET lifecycle_phase = 'runnable' \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seed a single active HMAC secret if none exists. The scheduler
/// refuses to issue grants against an empty keystore; smoke is
/// responsible for having a baseline row.
async fn ensure_hmac_secret(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ff_waitpoint_hmac (kid, secret, rotated_at_ms) \
         VALUES ('smoke-kid', decode('0000000000000000000000000000000000000000000000000000000000000000', 'hex'), 0) \
         ON CONFLICT (kid) DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn verify_terminal(
    pool: &PgPool,
    eid: &ff_core::types::ExecutionId,
) -> Result<bool, sqlx::Error> {
    let part_i16 = eid.partition() as i16;
    let uuid = parse_exec_uuid(eid);
    let phase: Option<String> = sqlx::query_scalar(
        "SELECT lifecycle_phase FROM ff_exec_core \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(uuid)
    .fetch_optional(pool)
    .await?;
    Ok(phase.as_deref() == Some("terminal"))
}

fn parse_exec_uuid(eid: &ff_core::types::ExecutionId) -> uuid::Uuid {
    let s = eid.as_str();
    let colon = s.rfind("}:").expect("hash-tag delimiter");
    uuid::Uuid::parse_str(&s[colon + 2..]).expect("uuid suffix")
}
