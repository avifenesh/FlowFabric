//! ff-dev — headline example for the RFC-023 SQLite dev-only backend
//! (v0.12.0).
//!
//! **Try FlowFabric in 60 seconds with zero Docker.** This example
//! spins up a `SqliteBackend` against an in-memory database, drives a
//! minimal end-to-end flow (create → claim → complete) through the
//! `EngineBackend` trait, and exercises a handful of Wave-9 admin /
//! read-side ops on top. Everything runs in-process; no network, no
//! container, no ambient services.
//!
//! # Run it
//!
//! ```text
//! FF_DEV_MODE=1 cargo run --bin ff-dev
//! ```
//!
//! # Scope
//!
//! Consumer-facing SQLite is **dev-only, permanently**. See
//! `rfcs/RFC-023-sqlite-dev-only-backend.md` (§1, §5) and
//! `docs/dev-harness.md` for the production-guard rationale.
//!
//! This example demonstrates the RFC-023 §4.7.1 cairn-canonical shape:
//! drive the backend trait directly, no ff-server, no ff-sdk worker
//! loop, no ferriskey in the dependency graph.

use std::sync::Arc;

use anyhow::{Context, Result};
use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{
    CapabilitySet, ClaimPolicy, ScannerFilter,
};
use ff_core::contracts::{
    AddExecutionToFlowArgs, CancelExecutionArgs, ChangePriorityArgs, CreateExecutionArgs,
    CreateExecutionResult, CreateFlowArgs, CreateFlowResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::stream_subscribe::StreamCursor;
use ff_core::types::{
    ExecutionId, FlowId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use tracing::info;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ff_dev=info,ff_backend_sqlite=warn".into()),
        )
        .init();

    // ── 1. Production-guard acknowledgement ────────────────────────────
    //
    // `SqliteBackend::new` refuses to construct without `FF_DEV_MODE=1`
    // in the process environment (RFC-023 §4.5). This example is
    // explicitly dev-only; fail loudly if the operator forgot.
    if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
        anyhow::bail!(
            "FF_DEV_MODE=1 is required. SQLite is dev-only; re-run with\n\
             `FF_DEV_MODE=1 cargo run --bin ff-dev`.\n\
             See rfcs/RFC-023-sqlite-dev-only-backend.md §3.3."
        );
    }

    // ── 2. Construct the backend ───────────────────────────────────────
    //
    // `:memory:` gives us an ephemeral, zero-setup database. The WARN
    // banner you see in the log is emitted by `SqliteBackend::new`
    // every time the type is constructed; operators use it to alert if
    // a SQLite backend ever reaches an environment it shouldn't.
    let db_uri = format!(
        "file:ff-dev-{}?mode=memory&cache=shared",
        Uuid::new_v4(),
    );
    let backend: Arc<SqliteBackend> = SqliteBackend::new(&db_uri)
        .await
        .context("construct SqliteBackend")?;
    let trait_obj: Arc<dyn EngineBackend> = backend.clone();

    // Side-pool dialed at the same shared-cache URI. `promote_to_runnable`
    // below uses it to flip one row's lifecycle column directly — a
    // dev-envelope convenience that substitutes for the scanner
    // supervisor's promotion pass. Real consumers using
    // `SqliteBackend::with_scanners` or the full ff-server boot path
    // never need this pool. Kept separate from the backend's own
    // connection pool so the example does not depend on the crate's
    // `#[doc(hidden)] pool_for_test()` hook.
    let side_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            db_uri
                .parse::<sqlx::sqlite::SqliteConnectOptions>()
                .context("parse dev URI for side-pool")?,
        )
        .await
        .context("open dev side-pool")?;

    let caps = trait_obj.capabilities();
    info!(
        family = caps.identity.family,
        version = %format!("{}.{}.{}", caps.identity.version.major, caps.identity.version.minor, caps.identity.version.patch),
        rfc017_stage = caps.identity.rfc017_stage,
        "dialed SQLite dev backend"
    );

    // RFC-023 §7.1: bundled libsqlite3-sys must ship SQLite >= 3.35
    // (JSON1 + RETURNING). Log the runtime-reported version on the
    // side-pool so smoke logs show which SQLite the bundled build
    // pulled in; parse + assert the floor so an accidental
    // libsqlite3-sys downgrade fails the smoke-gate loudly.
    let sqlite_version: String =
        sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&side_pool)
            .await
            .context("SELECT sqlite_version()")?;
    info!(version = %sqlite_version, "sqlite_version");
    {
        let mut parts = sqlite_version.split('.');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        anyhow::ensure!(
            major > 3 || (major == 3 && minor >= 35),
            "bundled SQLite {} is below RFC-023 §7.1 floor (3.35)",
            sqlite_version
        );
    }

    // ── 3. Create a flow + three executions ────────────────────────────
    //
    // These are the RFC-012 ingress-row trait methods: every backend
    // (Valkey, Postgres, SQLite) implements the same shape.
    let lane = LaneId::new("default");
    let flow_id = FlowId::from_uuid(Uuid::new_v4());
    let now = TimestampMs::from_millis(1_000);

    let flow_result = trait_obj
        .create_flow(CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "dag".into(),
            namespace: Namespace::new("default"),
            now,
        })
        .await?;
    info!(flow = %flow_id, ?flow_result, "create_flow");
    assert!(matches!(flow_result, CreateFlowResult::Created { .. }));

    let mut execs: Vec<ExecutionId> = Vec::with_capacity(3);
    for n in 0..3 {
        let exec_id = new_exec_id();
        let created = trait_obj
            .create_execution(create_execution_args(&exec_id, &lane, n * 100, now))
            .await?;
        assert!(matches!(created, CreateExecutionResult::Created { .. }));
        trait_obj
            .add_execution_to_flow(AddExecutionToFlowArgs {
                flow_id: flow_id.clone(),
                execution_id: exec_id.clone(),
                now,
            })
            .await?;
        info!(execution = %exec_id, priority = n * 100, "create_execution + add_execution_to_flow");
        execs.push(exec_id);
    }

    // ── 4. Drive one execution through claim → complete ────────────────
    //
    // `create_execution` inserts `lifecycle_phase='submitted'`,
    // `public_state='waiting'`, `attempt_state='pending'`. The claim
    // path requires `lifecycle_phase='runnable'` /
    // `public_state='pending'` / `attempt_state='initial'` — in the
    // steady-state Valkey / Postgres pipeline the scheduler + scanner
    // supervisor walk executions through that promotion. Under the
    // SQLite dev envelope the example flips the columns directly so
    // the trait-level claim / complete lifecycle is visible
    // end-to-end.
    let target = &execs[0];
    promote_to_runnable(&side_pool, target)
        .await
        .context("promote_to_runnable")?;

    let claim_policy = ClaimPolicy::new(
        WorkerId::new("ff-dev-worker"),
        WorkerInstanceId::new("ff-dev-worker-0"),
        30_000,
        None,
    );
    let handle = trait_obj
        .claim(&lane, &CapabilitySet::default(), claim_policy)
        .await
        .context("claim")?
        .context("claim returned None — unexpected under the dev-seed shape")?;
    info!(execution = %target, "claim → minted handle");

    trait_obj
        .complete(&handle, Some(b"done".to_vec()))
        .await
        .context("complete")?;
    info!(execution = %target, "complete");

    // ── 5. Wave-9 admin ops on the remaining executions ────────────────
    //
    // RFC-023 §4.3 commits full Wave-9 parity with Postgres v0.11:
    // `change_priority`, `cancel_execution`, `read_execution_info`, and
    // the subscribe surface all work identically.
    //
    // `change_priority` specifically requires the execution to already
    // be in the `runnable` phase (same contention guard as PG) — the
    // raw-SQL promote here is the dev-only analogue of what the
    // Valkey / Postgres scheduler does automatically. `cancel_execution`
    // has NO such requirement — it works on the freshly-created row
    // too — but we promote exec[2] as well so the follow-on
    // `read_execution_info` audit surfaces a state the wire-level
    // reader normalizer accepts.
    promote_to_runnable(&side_pool, &execs[1]).await?;
    promote_to_runnable(&side_pool, &execs[2]).await?;

    trait_obj
        .change_priority(ChangePriorityArgs {
            execution_id: execs[1].clone(),
            new_priority: 9_000,
            lane_id: lane.clone(),
            now: TimestampMs::from_millis(2_000),
        })
        .await?;
    info!(execution = %execs[1], new_priority = 9_000, "change_priority");

    trait_obj
        .cancel_execution(CancelExecutionArgs {
            execution_id: execs[2].clone(),
            reason: "ff-dev demo cancel".into(),
            source: Default::default(),
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::from_millis(3_000),
        })
        .await?;
    info!(execution = %execs[2], "cancel_execution");

    // ── 6. Read-model audit ────────────────────────────────────────────
    //
    // Only exercise `read_execution_info` for execs whose state has
    // traversed the reader-side normalizer: the completed exec[0] and
    // the cancelled exec[2]. `change_priority` alone leaves the row
    // in the pre-runnable `pending` shape, which the read path would
    // reject as `Corruption` (valid for Valkey / Postgres where the
    // scheduler promotes the row automatically, but the dev-seed path
    // skips that promotion).
    for ex in [&execs[0], &execs[2]] {
        if let Some(info_row) = trait_obj.read_execution_info(ex).await? {
            info!(
                execution = %ex,
                public_state = ?info_row.public_state,
                priority = info_row.priority,
                lane = %info_row.lane_id,
                "read_execution_info"
            );
        }
    }

    // ── 7. RFC-019 subscribe surface — cursor-resume handshake ─────────
    //
    // `subscribe_completion` exposes the same outbox-backed cursor
    // replay contract Postgres v0.11 ships. Here we just prove the
    // surface is live + attach to a cursor; production consumers persist
    // the cursor bytes across restarts.
    let filter = ScannerFilter::new();
    let _subscription = trait_obj
        .subscribe_completion(StreamCursor::empty(), &filter)
        .await
        .context("subscribe_completion")?;
    info!("subscribe_completion attached (RFC-019 cursor-resume surface live)");

    // ── 8. Clean shutdown ──────────────────────────────────────────────
    trait_obj
        .shutdown_prepare(std::time::Duration::from_millis(100))
        .await
        .context("shutdown_prepare")?;

    info!("ff-dev demo complete — SQLite dev backend ran end-to-end with zero Docker");
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

fn create_execution_args(
    exec_id: &ExecutionId,
    lane: &LaneId,
    priority: i32,
    now: TimestampMs,
) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: lane.clone(),
        execution_kind: "op".into(),
        input_payload: b"hello".to_vec(),
        payload_encoding: None,
        priority,
        creator_identity: "ff-dev".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now,
    }
}

/// Promote a freshly-created execution into the `runnable` /
/// `pending` / `initial` shape the claim path expects. In production
/// backends the scheduler + scanner supervisor do this; under the
/// SQLite dev envelope the caller drives the row directly so the
/// end-to-end lifecycle is visible in-example.
async fn promote_to_runnable(
    pool: &sqlx::SqlitePool,
    exec_id: &ExecutionId,
) -> Result<()> {
    let exec_uuid = Uuid::parse_str(
        exec_id
            .as_str()
            .split_once("}:")
            .context("execution id missing `{fp:N}:` hash-tag prefix")?
            .1,
    )
    .context("exec id tail not a UUID")?;
    sqlx::query(
        "UPDATE ff_exec_core \
            SET lifecycle_phase='runnable', public_state='pending', attempt_state='initial' \
          WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(pool)
    .await
    .context("UPDATE ff_exec_core")?;
    Ok(())
}
