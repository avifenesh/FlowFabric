//! v011-wave9-postgres — headline demo for the v0.11.0 Postgres Wave 9
//! release (RFC-020 Rev 7).
//!
//! Scenario: a minimal multi-tenant operator dashboard using the
//! Postgres backend directly. Every one of the six Wave-9 method
//! groups is exercised naturally:
//!
//! 1. Budget/quota admin (create_budget, create_quota_policy,
//!    report_usage_admin, get_budget_status) — tenant A + B
//!    provisioning.
//! 2. Operator control (change_priority) — operator bumps priority on
//!    a runnable tenant-A execution.
//! 3. Operator control (cancel_execution + ack_cancel_member) —
//!    operator cancels a stuck tenant-B execution.
//! 4. Operator control (replay_execution) — operator replays a
//!    terminally-failed tenant-A execution.
//! 5. Waitpoint listing (list_pending_waitpoints) — operator lists
//!    pending waitpoints for a tenant-A exec.
//! 6. Flow cancel split (cancel_flow_header) — operator cancels an
//!    entire tenant-A flow.
//!
//! Plus read-model (read_execution_info) at the end for an audit
//! summary on each exec.
//!
//! Because this example exercises admin/operator control paths
//! directly against the backend trait — not through the full
//! create/dispatch/claim hot path — it seeds lightweight ff_exec_core
//! + ff_attempt rows using the same pattern as the in-tree
//! integration tests.
//!
//! # Prereqs
//!
//! * Postgres reachable at FF_PG_TEST_URL.
//! * Database is fresh or already migrated to the v0.11 schema
//!   (migrations 0001 through 0014). The example calls
//!   apply_migrations at boot, so a brand-new empty db works.

use std::sync::Arc;

use anyhow::{Context, Result};
use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::contracts::{
    CancelExecutionArgs, CancelFlowArgs, CancelFlowHeader, ChangePriorityArgs, CreateBudgetArgs,
    CreateQuotaPolicyArgs, ListPendingWaitpointsArgs, ReplayExecutionArgs, ReportUsageAdminArgs,
    ReportUsageResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, QuotaPolicyId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "v011_wave9_postgres=info".into()),
        )
        .init();

    let url = std::env::var("FF_PG_TEST_URL")
        .context("FF_PG_TEST_URL not set — point at a Postgres for this demo")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .context("connect to FF_PG_TEST_URL")?;
    apply_migrations(&pool).await.context("apply_migrations")?;
    let partition_config = PartitionConfig::default();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), partition_config.clone());

    let caps = backend.capabilities();
    info!(family = caps.identity.family, rfc017_stage = caps.identity.rfc017_stage, "dialed Postgres backend");
    info!(
        cancel_execution = caps.supports.cancel_execution,
        change_priority = caps.supports.change_priority,
        replay_execution = caps.supports.replay_execution,
        cancel_flow_header = caps.supports.cancel_flow_header,
        ack_cancel_member = caps.supports.ack_cancel_member,
        list_pending_waitpoints = caps.supports.list_pending_waitpoints,
        budget_admin = caps.supports.budget_admin,
        quota_admin = caps.supports.quota_admin,
        read_execution_info = caps.supports.read_execution_info,
        "Wave 9 capability flags — all true on v0.11+"
    );

    // ── (1) Budget/quota admin — tenant provisioning ───────────────
    let tenant_a_budget = BudgetId::new();
    let tenant_b_budget = BudgetId::new();
    let quota = QuotaPolicyId::new();
    let now = TimestampMs::now();

    backend.create_budget(budget_args(&tenant_a_budget, "tenant-a", 10_000, 8_000, now)).await?;
    backend.create_budget(budget_args(&tenant_b_budget, "tenant-b", 5_000, 4_000, now)).await?;
    backend
        .create_quota_policy(CreateQuotaPolicyArgs {
            quota_policy_id: quota.clone(),
            window_seconds: 60,
            max_requests_per_window: 1_000,
            max_concurrent: 50,
            now,
        })
        .await?;
    let r = backend
        .report_usage_admin(
            &tenant_a_budget,
            ReportUsageAdminArgs::new(vec!["tokens".into()], vec![6_000], now),
        )
        .await?;
    info!(?r, "tenant-a report_usage_admin");
    match r {
        ReportUsageResult::Ok | ReportUsageResult::SoftBreach { .. } => {}
        _ => anyhow::bail!("unexpected budget verdict"),
    }
    let status_a = backend.get_budget_status(&tenant_a_budget).await?;
    info!(scope = %status_a.scope_id, usage = ?status_a.usage, soft_breaches = status_a.soft_breach_count, "tenant-a budget status");

    // ── Seed three tenant executions (one per operator action) ────
    let lane = LaneId::new("default");
    let exec_priority = seed_runnable_exec(&pool, &partition_config, &lane, None, 100, now).await?;
    let exec_cancel = seed_runnable_exec(&pool, &partition_config, &lane, None, 200, now).await?;
    let exec_replay = seed_failed_exec(&pool, &partition_config, &lane, now).await?;

    // ── (2) change_priority — tenant-A hot exec bumped ─────────────
    backend
        .change_priority(ChangePriorityArgs {
            execution_id: exec_priority.clone(),
            new_priority: 9_000,
            lane_id: lane.clone(),
            now: TimestampMs::now(),
        })
        .await?;
    info!(execution = %exec_priority, new_priority = 9_000, "operator change_priority ok");

    // ── (3) cancel_execution + ack_cancel_member surface ──────────
    backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: exec_cancel.clone(),
            reason: "operator stuck-exec sweep".into(),
            source: Default::default(),
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await?;
    info!(execution = %exec_cancel, "operator cancel_execution ok");
    info!("ack_cancel_member surface live (driven by cancel-backlog reconciler in steady state)");

    // ── (4) replay_execution — revive a failed exec ────────────────
    backend
        .replay_execution(ReplayExecutionArgs {
            execution_id: exec_replay.clone(),
            now: TimestampMs::now(),
        })
        .await?;
    info!(execution = %exec_replay, "operator replay_execution ok");

    // ── (5) list_pending_waitpoints — scoped to one exec ───────────
    let wps = backend
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(exec_priority.clone()))
        .await?;
    info!(execution = %exec_priority, entries = wps.entries.len(), "list_pending_waitpoints ok");

    // ── (6) cancel_flow_header — end-to-end flow cancel ────────────
    let flow = FlowId::new();
    let flow_uuid = flow.0;
    seed_flow(&pool, &partition_config, &flow, flow_uuid, now).await?;
    let flow_exec_1 = seed_runnable_exec(&pool, &partition_config, &lane, Some((flow_uuid, &flow)), 100, now).await?;
    let flow_exec_2 = seed_runnable_exec(&pool, &partition_config, &lane, Some((flow_uuid, &flow)), 100, now).await?;
    let header = backend
        .cancel_flow_header(CancelFlowArgs {
            flow_id: flow.clone(),
            reason: "operator kill-switch".into(),
            cancellation_policy: "cancel_all".into(),
            now: TimestampMs::now(),
        })
        .await?;
    let member_ids: Vec<String> = match &header {
        CancelFlowHeader::Cancelled { member_execution_ids, .. } => {
            info!(flow = %flow, members = member_execution_ids.len(), "cancel_flow_header ok");
            member_execution_ids.clone()
        }
        CancelFlowHeader::AlreadyTerminal { member_execution_ids, .. } => {
            info!(flow = %flow, "cancel_flow_header AlreadyTerminal (idempotent)");
            member_execution_ids.clone()
        }
        _ => Vec::new(),
    };
    // ack_cancel_member drain — operator-side drain of the
    // cancel-backlog rows seeded by cancel_flow_header. In steady
    // state this fires from the cancel-backlog reconciler; here we
    // invoke it directly so the surface is demonstrated.
    for mid in &member_ids {
        let exec = ExecutionId::parse(mid).context("parse member execution id")?;
        backend.ack_cancel_member(&flow, &exec).await?;
    }
    info!(flow = %flow, drained = member_ids.len(), "ack_cancel_member drain ok");

    // ── read_execution_info — audit across the execs ──────────────
    for ex in [&exec_priority, &exec_cancel, &exec_replay, &flow_exec_1, &flow_exec_2] {
        if let Some(info_row) = backend.read_execution_info(ex).await? {
            info!(
                execution = %ex,
                public_state = ?info_row.public_state,
                priority = info_row.priority,
                lane = %info_row.lane_id,
                "read_execution_info audit"
            );
        }
    }

    info!("demo complete — all 6 Wave-9 method groups exercised on Postgres v0.11.0");
    Ok(())
}

// ─── Fixture helpers ──────────────────────────────────────────────

fn budget_args(id: &BudgetId, tenant: &str, hard_tokens: u64, soft_tokens: u64, now: TimestampMs) -> CreateBudgetArgs {
    CreateBudgetArgs {
        budget_id: id.clone(),
        scope_type: "flow".into(),
        scope_id: tenant.into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![hard_tokens],
        soft_limits: vec![soft_tokens],
        now,
    }
}

fn exec_uuid_of(id: &ExecutionId) -> Uuid {
    Uuid::parse_str(id.as_str().split_once("}:").unwrap().1).unwrap()
}

async fn seed_runnable_exec(
    pool: &PgPool,
    config: &PartitionConfig,
    lane: &LaneId,
    flow: Option<(Uuid, &FlowId)>,
    priority: i32,
    now: TimestampMs,
) -> Result<ExecutionId> {
    let eid = match flow {
        Some((_uuid, fid)) => ExecutionId::for_flow(fid, config),
        None => ExecutionId::solo(lane, config),
    };
    let part = eid.partition() as i16;
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, flow_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, priority, created_at_ms, raw_fields) \
         VALUES ($1, $2, $3, $4, 0, 'runnable', 'unowned', 'eligible_now', \
                 'waiting', 'pending', $5, $6, '{}'::jsonb)",
    )
    .bind(part)
    .bind(exec_uuid_of(&eid))
    .bind(flow.map(|(u, _)| u))
    .bind(lane.as_str())
    .bind(priority)
    .bind(now.0)
    .execute(pool)
    .await
    .context("seed runnable exec")?;
    Ok(eid)
}

async fn seed_failed_exec(pool: &PgPool, config: &PartitionConfig, lane: &LaneId, now: TimestampMs) -> Result<ExecutionId> {
    let eid = ExecutionId::solo(lane, config);
    let part = eid.partition() as i16;
    let uuid = exec_uuid_of(&eid);
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, priority, created_at_ms, raw_fields) \
         VALUES ($1, $2, $3, 0, 'terminal', 'unowned', 'not_applicable', \
                 'failed', 'terminated', 100, $4, '{}'::jsonb)",
    )
    .bind(part)
    .bind(uuid)
    .bind(lane.as_str())
    .bind(now.0)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, lease_epoch, outcome) \
         VALUES ($1, $2, 0, 1, 'failed')",
    )
    .bind(part)
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(eid)
}

async fn seed_flow(pool: &PgPool, config: &PartitionConfig, flow: &FlowId, flow_uuid: Uuid, now: TimestampMs) -> Result<()> {
    let part = ff_core::partition::flow_partition(flow, config).index as i16;
    sqlx::query(
        "INSERT INTO ff_flow_core \
           (partition_key, flow_id, public_flow_state, created_at_ms, raw_fields) \
         VALUES ($1, $2, 'active', $3, '{}'::jsonb) \
         ON CONFLICT DO NOTHING",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(now.0)
    .execute(pool)
    .await?;
    Ok(())
}
