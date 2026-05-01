//! external-callback — SC-09 headline example for the v0.13 waitpoint
//! consumer surface.
//!
//! # Scenario
//!
//! A workflow needs to wait for an asynchronous external actor
//! (webhook, email reply, manual approval, third-party API callback).
//! The flow suspends against a pending waitpoint with an HMAC-signed
//! token; the external actor POSTs the token back; a signal-bridge
//! verifies the token and resumes the workflow.
//!
//! Three steps: `request_approval` → `wait_for_external` → `finalize`.
//!
//! 1. `request_approval` creates a pending waitpoint with a 5-minute
//!    TTL and logs the HMAC token (simulates "we emailed the approver").
//! 2. The workflow calls `suspend` bound to the pending waitpoint and
//!    releases the lease.
//! 3. A separate "external-actor" task waits 5 seconds (simulating
//!    human round-trip), then POSTs the token back. The "signal-bridge"
//!    shim reads the stored token via
//!    [`EngineBackend::read_waitpoint_token`] (#434) to authenticate
//!    the POST; a tamper-path run shows the mismatch rejection.
//! 4. The bridge calls [`FlowFabricWorker::deliver_signal`] with the
//!    verified token; the engine satisfies the resume condition;
//!    `claim_resumed_execution` mints a fresh handle; `finalize` runs.
//!
//! # v0.13 surface exercised
//!
//! | Surface                                      | Cairn issue |
//! |----------------------------------------------|-------------|
//! | `EngineBackend::create_waitpoint`            | #435        |
//! | `EngineBackend::read_waitpoint_token`        | #434        |
//! | `FlowFabricAdminClient::connect_with`        | #432        |
//! | `WorkerConfig.backend: Option<BackendConfig>`| #448        |
//!
//! # Run
//!
//! ```text
//! # SQLite (zero-infra, end-to-end):
//! FF_DEV_MODE=1 cargo run -p external-callback -- --backend sqlite
//!
//! # Valkey / Postgres (require ff-server + scheduler — see README):
//! cargo run -p external-callback -- --backend valkey
//! cargo run -p external-callback -- --backend postgres
//! ```
//!
//! # Migration reference
//!
//! [`docs/CONSUMER_MIGRATION_0.13.md`](../../docs/CONSUMER_MIGRATION_0.13.md)
//! documents the production migration pattern this scenario dramatises.
//!
//! [`EngineBackend::read_waitpoint_token`]: ff_core::engine_backend::EngineBackend::read_waitpoint_token
//! [`FlowFabricWorker::deliver_signal`]: ff_sdk::FlowFabricWorker::deliver_signal

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ff_core::backend::{CapabilitySet, ClaimPolicy, PendingWaitpoint};
use ff_core::contracts::{
    AddExecutionToFlowArgs, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    CreateExecutionArgs, CreateFlowArgs, ResumeCondition, ResumePolicy,
    SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{
    AttemptIndex, ExecutionId, FlowId, LaneId, LeaseId, Namespace, SuspensionId, TimestampMs,
    WaitpointToken, WorkerId, WorkerInstanceId,
};
use ff_sdk::admin::FlowFabricAdminClient;
use ff_sdk::signal_bridge::{self, SignalBridgeError};
use ff_sdk::task::Signal;
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use tracing::info;
use uuid::Uuid;

/// Waitpoint TTL — 5 minutes as per the narrative. Not load-bearing in
/// the SQLite demo (the external-actor fires within seconds); the long
/// TTL is a production-realistic value the example doesn't need to hit.
const WAITPOINT_TTL_MS: u64 = 5 * 60 * 1_000;

/// Worker lease TTL. 30s matches `examples/incident-remediation`.
const LEASE_TTL_MS: u64 = 30_000;

/// How long the external actor "thinks" before POSTing back. Keeps the
/// scenario watchable end-to-end without stretching past a test budget.
const EXTERNAL_ACTOR_DELAY_MS: u64 = 2_000;

const KID: &str = "kid-external-callback";
fn secret_hex() -> String {
    // 32-byte HMAC key. Same shape the `ff-backend-sqlite` tests use.
    "cafebabe".repeat(8)
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Backend {
    Sqlite,
    Postgres,
    Valkey,
}

#[derive(Parser, Debug)]
#[command(about = "SC-09 external-callback — v0.13 waitpoint surface demo")]
struct Args {
    /// Which backend to run the scenario against.
    #[arg(long, value_enum, default_value_t = Backend::Sqlite)]
    backend: Backend,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ff_backend_sqlite=warn".into()),
        )
        .init();

    let args = Args::parse();
    info!(target: "engine", backend = ?args.backend, "[engine] SC-09 external-callback starting");

    match args.backend {
        Backend::Sqlite => run_sqlite().await,
        Backend::Postgres | Backend::Valkey => {
            anyhow::bail!(
                "--backend {:?} requires a live ff-server + scheduler deployment; see README.\n\
                 For end-to-end with zero infra, re-run with\n\
                 `FF_DEV_MODE=1 cargo run -p external-callback -- --backend sqlite`.",
                args.backend
            );
        }
    }
}

// ────────────────────────────── SQLite end-to-end ──────────────────────────────

async fn run_sqlite() -> Result<()> {
    if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
        anyhow::bail!(
            "FF_DEV_MODE=1 is required for --backend sqlite. Re-run with\n\
             `FF_DEV_MODE=1 cargo run -p external-callback -- --backend sqlite`."
        );
    }

    let db_uri = format!("file:sc09-{}?mode=memory&cache=shared", Uuid::new_v4());
    let backend = ff_backend_sqlite::SqliteBackend::new(&db_uri)
        .await
        .context("construct SqliteBackend")?;
    let trait_obj: Arc<dyn EngineBackend> = backend.clone();

    // v0.13 #432: admin facade constructed with `connect_with`. No
    // `ff-server` running — the embedded trait-dispatch transport
    // handles `read_waitpoint_token` for the signal-bridge shim.
    let admin = FlowFabricAdminClient::connect_with(trait_obj.clone());
    info!(target: "engine", "[engine] SQLite dev backend + admin facade ready");

    // Seed the HMAC keystore so `create_waitpoint` / `suspend` mint
    // real tokens (the RFC-004 §Waitpoint Security contract).
    trait_obj
        .seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .context("seed_waitpoint_hmac_secret")?;

    let side_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            db_uri
                .parse::<sqlx::sqlite::SqliteConnectOptions>()
                .context("parse dev URI for side-pool")?,
        )
        .await
        .context("open side-pool")?;

    // ── 1. Provision the approval execution ──────────────────────────
    let namespace = Namespace::new("approvals");
    let lane = LaneId::new("callback");
    let flow_id = FlowId::from_uuid(Uuid::new_v4());
    let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id");
    let now = TimestampMs::from_millis(1_000);

    trait_obj
        .create_flow(CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "approval".into(),
            namespace: namespace.clone(),
            now,
        })
        .await?;
    trait_obj
        .create_execution(CreateExecutionArgs {
            execution_id: exec_id.clone(),
            namespace: namespace.clone(),
            lane_id: lane.clone(),
            execution_kind: "approve_deploy".into(),
            input_payload: b"deploy svc-billing@v42 to prod".to_vec(),
            payload_encoding: None,
            priority: 10_000,
            creator_identity: "pipeline".into(),
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
    info!(target: "engine", execution = %exec_id, "[engine] approval execution filed");

    // ── 2. Build a worker via `connect_with` (exercises #448) ────────
    let worker = build_worker(trait_obj.clone(), &lane).await?;

    // ── 3. Step `request_approval` — claim, create waitpoint, suspend ─
    let policy = ClaimPolicy::new(
        WorkerId::new("approval-worker"),
        WorkerInstanceId::new("approval-worker-0"),
        LEASE_TTL_MS as u32,
        None,
    );
    let handle = trait_obj
        .claim(&lane, &CapabilitySet::default(), policy)
        .await?
        .context("claim returned None — unexpected under dev-seed shape")?;
    info!(target: "workflow", step = "request_approval", "[workflow] step 1/3: request_approval — claimed execution");

    let pending: PendingWaitpoint = trait_obj
        .create_waitpoint(&handle, "wpk:external-callback", Duration::from_millis(WAITPOINT_TTL_MS))
        .await
        .context("create_waitpoint")?;
    let waitpoint_id = pending.waitpoint_id.clone();
    let issued_token: WaitpointToken = pending.hmac_token.token().clone();
    info!(
        target: "workflow",
        waitpoint_id = %waitpoint_id,
        token_kid = %issued_token.as_str().split(':').next().unwrap_or(""),
        "[workflow] minted pending waitpoint — token dispatched to external actor (simulated email)"
    );

    // Suspend with the pending binding + wildcard matcher. Any signal
    // on this waitpoint satisfies the resume.
    let suspend_args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::use_pending(&pending),
        ResumeCondition::Single {
            waitpoint_key: "wpk:external-callback".to_owned(),
            matcher: SignalMatcher::Wildcard,
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForCallback,
        TimestampMs::now(),
    );
    let suspend_outcome = trait_obj.suspend(&handle, suspend_args).await?;
    match suspend_outcome {
        SuspendOutcome::Suspended { .. } => {
            info!(target: "workflow", step = "wait_for_external", "[workflow] step 2/3: wait_for_external — suspended, lease released");
        }
        SuspendOutcome::AlreadySatisfied { .. } => {
            anyhow::bail!("unexpected AlreadySatisfied on fresh waitpoint")
        }
        _ => anyhow::bail!("unexpected SuspendOutcome variant"),
    }

    // ── 4. External actor + signal-bridge ────────────────────────────
    //
    // The "external actor" represents a webhook callback / email reply
    // / third-party API. It sleeps `EXTERNAL_ACTOR_DELAY_MS` then POSTs
    // the token back to the "signal-bridge" — here simulated in-process
    // by directly calling the bridge shim.
    info!(target: "external-actor", delay_ms = EXTERNAL_ACTOR_DELAY_MS, "[external-actor] approver thinking…");
    tokio::time::sleep(Duration::from_millis(EXTERNAL_ACTOR_DELAY_MS)).await;

    // Demonstrate the tamper-rejection branch BEFORE the happy path:
    // flip one hex char in the issued token and watch the bridge reject.
    let tampered = tamper(&issued_token);
    info!(target: "external-actor", "[external-actor] (adversary attempt — posting with tampered token)");
    match signal_bridge::verify_and_deliver_arc(
        &trait_obj,
        &worker,
        &exec_id,
        &waitpoint_id,
        &tampered,
        callback_signal(tampered.clone()),
    )
    .await
    {
        Err(SignalBridgeError::TokenMismatch) => {
            info!(target: "signal-bridge", "[signal-bridge] REJECT — token mismatch (HMAC verify failed); adversary foiled");
        }
        Err(other) => anyhow::bail!("expected TokenMismatch, got {other:?}"),
        Ok(_) => anyhow::bail!("tampered token accepted — HMAC verification is broken"),
    }

    // Happy path: real token.
    info!(target: "external-actor", payload = "{approved: true}", "[external-actor] POST /callback with valid token");
    signal_bridge::verify_and_deliver_arc(
        &trait_obj,
        &worker,
        &exec_id,
        &waitpoint_id,
        &issued_token,
        callback_signal(issued_token.clone()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("signal-bridge deliver: {e}"))?;
    info!(target: "signal-bridge", "[signal-bridge] ACCEPT — token verified, deliver_signal dispatched, resume condition satisfied");

    // ── 5. Step `finalize` — claim the resumed execution, complete ───
    let resume_args = ClaimResumedExecutionArgs {
        execution_id: exec_id.clone(),
        worker_id: WorkerId::new("approval-worker"),
        worker_instance_id: WorkerInstanceId::new("approval-worker-0"),
        lane_id: lane.clone(),
        lease_id: LeaseId::new(),
        lease_ttl_ms: LEASE_TTL_MS,
        current_attempt_index: AttemptIndex::new(0),
        remaining_attempt_timeout_ms: None,
        now: TimestampMs::now(),
    };
    let resumed = trait_obj.claim_resumed_execution(resume_args).await?;
    let ClaimResumedExecutionResult::Claimed(claimed) = resumed;
    info!(
        target: "workflow",
        step = "finalize",
        execution = %claimed.execution_id,
        "[workflow] step 3/3: finalize — re-claimed resumed execution"
    );
    trait_obj
        .complete(&claimed.handle, Some(b"deploy approved".to_vec()))
        .await
        .context("complete")?;
    info!(target: "workflow", "[workflow] complete — approval workflow finished");
    info!(target: "engine", "[engine] SC-09 scenario complete — waitpoint + signal-bridge round-trip demonstrated");

    // Silence unused warnings on `admin` — the v0.13 narrative is the
    // facade's mere existence (cairn migration from HTTP-only to
    // embedded). `issue_reclaim_grant` / `claim_for_worker` are out of
    // scope for SC-09 but the facade IS the consumer entry point a
    // real operator would reach for (see docs/CONSUMER_MIGRATION_0.13.md).
    let _ = admin;
    let _ = worker;
    Ok(())
}

// ────────────────────────────── signal-bridge payload ──────────────────────────────
//
// The authentication + delivery shape (`verify_and_deliver` +
// `SignalBridgeError`) lives in `ff_sdk::signal_bridge` as of v0.14;
// this example constructs the domain-specific `Signal` payload the
// bridge forwards on a successful HMAC check. Real callback bridges
// would vary the signal name / category / source per integration.

fn callback_signal(token: WaitpointToken) -> Signal {
    Signal {
        signal_name: "callback".to_owned(),
        signal_category: "external".to_owned(),
        payload: Some(b"{\"approved\": true}".to_vec()),
        source_type: "webhook".to_owned(),
        source_identity: "approver@example.com".to_owned(),
        idempotency_key: None,
        // Overwritten by the bridge (see ff_sdk::signal_bridge rustdoc);
        // supplied here for struct-completeness only.
        waitpoint_token: token,
    }
}

/// Flip one hex character of the signed digest so the HMAC compare
/// fails. Keeps the `<kid>:` prefix intact — a realistic tamper pattern
/// (attacker preserves structure, mutates payload). The token shape is
/// `<kid>:<hex-digest>`; we split on the first `:` and mutate only the
/// digest half so kids like `kid-external-callback` (which contain
/// ASCII hex digits themselves) aren't accidentally rewritten.
fn tamper(token: &WaitpointToken) -> WaitpointToken {
    let raw = token.as_str();
    let Some((kid, digest)) = raw.split_once(':') else {
        // No kid prefix — fall back to flipping the first hex char in
        // the whole string. Shouldn't happen on a well-formed token.
        return WaitpointToken::new(flip_first_hex(raw));
    };
    WaitpointToken::new(format!("{kid}:{}", flip_first_hex(digest)))
}

fn flip_first_hex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut flipped = false;
    for c in s.chars() {
        if !flipped && c.is_ascii_hexdigit() {
            out.push(if c == '0' { 'f' } else { '0' });
            flipped = true;
        } else {
            out.push(c);
        }
    }
    out
}

// ────────────────────────────── helpers ──────────────────────────────

async fn build_worker(
    backend: Arc<dyn EngineBackend>,
    lane: &LaneId,
) -> Result<FlowFabricWorker> {
    // v0.13 #448: `WorkerConfig.backend` is `Option<BackendConfig>`.
    // Under `connect_with` the injected backend is authoritative; the
    // `backend` field is `None` (pre-v0.13 required a placeholder).
    let config = WorkerConfig {
        backend: None,
        worker_id: WorkerId::new("approval-worker"),
        worker_instance_id: WorkerInstanceId::new("approval-worker-0"),
        namespace: Namespace::new("approvals"),
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

fn uuid_of(eid: &ExecutionId) -> Uuid {
    Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
}

/// Promote a freshly-created execution into the `runnable` / `pending`
/// / `initial` shape the claim path expects. Dev-only analogue of the
/// scheduler+scanner promotion pass. Mirrors `examples/ff-dev` +
/// `examples/incident-remediation`.
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
