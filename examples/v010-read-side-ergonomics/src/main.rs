//! v010-read-side-ergonomics — headline demo for the v0.10.0 consumer
//! read-side API surface.
//!
//! Three shipping-in-v0.10.0 features are exercised end-to-end against
//! a live ff-server + Valkey deployment:
//!
//! 1. **Flat [`ff_core::capability::Supports`] discovery (#277).**
//!    `backend.capabilities()` returns a [`Capabilities`] whose
//!    `supports` field is a flat named-field struct. Consumers dot-
//!    access the bools (`caps.supports.subscribe_lease_history`) rather
//!    than walking a map of enum keys. Before dispatching a
//!    subscription we `match` on these bools and abort early with a
//!    clear message if the backend cannot serve the call.
//!
//! 2. **Typed [`LeaseHistoryEvent`] stream (#282).** Instead of the
//!    v0.9 `StreamEvent { payload: Bytes, .. }` envelope that forced
//!    every consumer to parse a NUL-delimited byte map,
//!    `subscribe_lease_history` now yields
//!    [`ff_core::stream_events::LeaseHistoryEvent`] directly. We
//!    `match` on `Acquired` / `Renewed` / `Expired` / `Reclaimed` /
//!    `Revoked` variants and render a per-tenant audit log.
//!
//! 3. **[`ScannerFilter`] tag-restricted subscription (#282).**
//!    A second subscriber is wired with
//!    `ScannerFilter::new().with_instance_tag("tenant", "acme")` and
//!    only observes lease history for executions that were created
//!    with the matching tag. This models a multi-tenant lease-audit
//!    console where each tenant's UI panel subscribes to its own
//!    slice of the global event stream without server-side routing.
//!
//! The example does NOT exercise `subscribe_completion` or
//! `subscribe_signal_delivery`; their API shape is identical to
//! `subscribe_lease_history` (same filter param, same flat-bool
//! capability gate, same non_exhaustive enum), so one well-narrated
//! family is enough for the headline doc.
//!
//! # Prereqs
//!
//! * `valkey-server` reachable at `FF_HOST:FF_PORT` (default
//!   `localhost:6379`).
//! * `ff-server` reachable at `FF_SERVER_URL` (default
//!   `http://localhost:9090`).
//!
//! # Run
//!
//! ```text
//! cargo run -p v010-read-side-ergonomics-example
//! ```
//!
//! See `README.md` in this directory for the expected log transcript.

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::{BackendConfig, ScannerFilter};
use ff_core::capability::Capabilities;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::policy::ExecutionPolicy;
use ff_core::stream_events::LeaseHistoryEvent;
use ff_core::stream_subscribe::StreamCursor;
use ff_core::types::{ExecutionId, LaneId, Namespace, WorkerId, WorkerInstanceId};
use ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::{info, warn};

const NAMESPACE: &str = "demo";
const LANE: &str = "default";
const WORKER_POOL: &str = "v010-read-side-worker";
const TAG_KEY: &str = "tenant";
const TAG_ACME: &str = "acme";
const TAG_CONTOSO: &str = "contoso";

// Bounded self-termination. The two executions run to success
// (handler is not flaky) so the demo finishes in the low seconds.
const DEMO_BUDGET: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NoopPayload {
    tenant: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "v010_read_side_ergonomics=info,ff_sdk=warn".into()),
        )
        .init();

    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    let server_url =
        std::env::var("FF_SERVER_URL").unwrap_or_else(|_| "http://localhost:9090".into());
    let partition_config = PartitionConfig::default();

    // ── Step 1: dial the backend via the worker SDK ───────────────
    //
    // `FlowFabricWorker::connect` owns the `Arc<dyn EngineBackend>`
    // under the hood; we borrow it via `worker.backend()` for the
    // read-side demos below, and spawn `run_worker_loop` to drive the
    // executions to terminal so the lease-history stream actually
    // produces events.
    let done = Arc::new(Notify::new());
    let worker = FlowFabricWorker::connect(WorkerConfig {
        backend: BackendConfig::valkey(host.clone(), port),
        worker_id: WorkerId::new(WORKER_POOL),
        worker_instance_id: WorkerInstanceId::new(format!(
            "{WORKER_POOL}-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: Namespace::new(NAMESPACE),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 500,
        max_concurrent_tasks: 2,
    })
    .await?;
    let backend: Arc<dyn EngineBackend> = worker
        .backend()
        .ok_or("worker has no engine backend handle")?
        .clone();

    // ── Step 2: capability discovery (#277) ────────────────────────
    //
    // Branch on the flat `Supports` struct before dispatching. If the
    // backend reports `subscribe_lease_history = false` we bail out
    // with a clear consumer-visible error instead of blowing up on
    // `EngineError::Unavailable` inside the stream.
    let caps: Capabilities = backend.capabilities();
    info!(
        family = caps.identity.family,
        version = ?caps.identity.version,
        rfc017_stage = caps.identity.rfc017_stage,
        "backend.capabilities() — identity"
    );
    info!(
        subscribe_lease_history = caps.supports.subscribe_lease_history,
        subscribe_completion = caps.supports.subscribe_completion,
        subscribe_signal_delivery = caps.supports.subscribe_signal_delivery,
        cancel_execution = caps.supports.cancel_execution,
        "backend.capabilities() — supports (flat struct, no enum + no map lookup)"
    );
    if !caps.supports.subscribe_lease_history {
        return Err(format!(
            "backend {} does not advertise subscribe_lease_history — this example needs it",
            caps.identity.family
        )
        .into());
    }

    // ── Step 3: spawn the two subscribers ─────────────────────────
    //
    // Subscriber A: unfiltered. Sees every lease-history event this
    // ff-server emits during the demo window.
    //
    // Subscriber B: tag-filtered to `tenant=acme`. Sees only events
    // for executions that carried that tag on create_execution.
    let admin = Arc::new(FlowFabricAdminClient::new(&server_url)?);

    let sub_done = Arc::new(Notify::new());
    let sub_all_handle = {
        let backend = backend.clone();
        let sub_done = Arc::clone(&sub_done);
        tokio::spawn(async move {
            run_subscriber(backend, "all", ScannerFilter::default(), 4, sub_done).await
        })
    };
    let sub_acme_handle = {
        let backend = backend.clone();
        let sub_done = Arc::clone(&sub_done);
        tokio::spawn(async move {
            run_subscriber(
                backend,
                "acme-only",
                ScannerFilter::new().with_instance_tag(TAG_KEY, TAG_ACME),
                2,
                sub_done,
            )
            .await
        })
    };

    // Give subscribers a brief moment to register their XREAD BLOCK
    // cursor before we submit work that produces events.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // ── Step 4: spawn the worker loop ─────────────────────────────
    let worker_handle = {
        let admin = Arc::clone(&admin);
        let done = Arc::clone(&done);
        tokio::spawn(async move { run_worker_loop(worker, admin, done).await })
    };

    // ── Step 5: submit two tagged executions and wait for terminal ─
    let http = reqwest::Client::new();
    let eid_acme = submit_tagged(&http, &server_url, &partition_config, TAG_ACME).await?;
    let eid_contoso = submit_tagged(&http, &server_url, &partition_config, TAG_CONTOSO).await?;
    info!(acme = %eid_acme, contoso = %eid_contoso, "submitted two executions with tenant tags");

    let work = timeout(DEMO_BUDGET, async {
        poll_until_terminal(&http, &server_url, &eid_acme).await?;
        poll_until_terminal(&http, &server_url, &eid_contoso).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;

    // ── Step 6: drain subscribers + shut down worker ──────────────
    //
    // Executions are already terminal. Give the lease-history
    // subscribers a short drain window past the terminal poll
    // (events are producer-timestamped; the consumer reads them on
    // the next XREAD round), then signal them to exit so the
    // demo returns promptly.
    tokio::time::sleep(Duration::from_secs(2)).await;
    sub_done.notify_waiters();
    done.notify_waiters();
    info!("awaiting worker shutdown");
    let _ = timeout(Duration::from_secs(5), worker_handle).await;
    info!("worker joined");

    let all_events = timeout(Duration::from_secs(7), sub_all_handle)
        .await
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();
    let acme_events = timeout(Duration::from_secs(7), sub_acme_handle)
        .await
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();

    info!(
        all = all_events.len(),
        acme = acme_events.len(),
        "subscriber drain complete"
    );
    info!("── all-subscriber events (unfiltered) ──");
    for ev in &all_events {
        info!(execution_id = %ev.execution_id(), variant = lease_event_name(ev), "event");
    }
    info!("── acme-only subscriber events (ScannerFilter instance_tag) ──");
    for ev in &acme_events {
        info!(execution_id = %ev.execution_id(), variant = lease_event_name(ev), "event");
    }

    // ── Step 7: assert tag-filter correctness ─────────────────────
    //
    // Every event the acme-only subscriber saw MUST belong to the
    // acme execution; contoso events must be absent.
    let contoso_id_str = eid_contoso.to_string();
    for ev in &acme_events {
        if ev.execution_id().to_string() == contoso_id_str {
            return Err(format!(
                "tag-filter leak: acme-only subscriber observed contoso execution {contoso_id_str}"
            )
            .into());
        }
    }
    if acme_events.is_empty() {
        warn!("acme subscriber observed 0 events — did the demo hit its budget before leases acquired?");
    }

    match work {
        Ok(Ok(())) => {
            info!("demo complete — capabilities() read, typed LeaseHistoryEvent match, ScannerFilter isolation verified");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "demo exceeded {}s budget (is ff-server at {server_url} reachable?)",
            DEMO_BUDGET.as_secs()
        )
        .into()),
    }
}

// ─── Subscribers ──────────────────────────────────────────────────

/// Spin up a typed lease-history subscription, collect up to
/// `want` events (or until the stream closes), return them. The
/// stream is typed `LeaseHistoryEvent` — no byte parsing.
async fn run_subscriber(
    backend: Arc<dyn EngineBackend>,
    label: &'static str,
    filter: ScannerFilter,
    want: usize,
    shutdown: Arc<Notify>,
) -> Vec<LeaseHistoryEvent> {
    let mut collected = Vec::with_capacity(want);
    let mut stream = match backend
        .subscribe_lease_history(StreamCursor::empty(), &filter)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(label, error = %e, "subscribe_lease_history failed");
            return collected;
        }
    };
    info!(label, filtered = !filter.is_noop(), "subscriber started");
    let deadline = tokio::time::Instant::now() + DEMO_BUDGET;
    loop {
        if collected.len() >= want || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::select! {
            _ = shutdown.notified() => {
                info!(label, "subscriber shutdown signalled");
                break;
            }
            item = timeout(Duration::from_secs(2), stream.next()) => {
                match item {
                    Ok(Some(Ok(ev))) => {
                        info!(
                            label,
                            variant = lease_event_name(&ev),
                            execution_id = %ev.execution_id(),
                            "subscriber yielded typed event"
                        );
                        collected.push(ev);
                    }
                    Ok(Some(Err(e))) => {
                        warn!(label, error = %e, "stream error");
                        break;
                    }
                    Ok(None) => {
                        info!(label, "stream ended");
                        break;
                    }
                    Err(_) => {
                        // Idle — loop around, check shutdown + deadline.
                    }
                }
            }
        }
    }
    info!(label, collected = collected.len(), "subscriber exit");
    collected
}

fn lease_event_name(ev: &LeaseHistoryEvent) -> &'static str {
    match ev {
        LeaseHistoryEvent::Acquired { .. } => "Acquired",
        LeaseHistoryEvent::Renewed { .. } => "Renewed",
        LeaseHistoryEvent::Expired { .. } => "Expired",
        LeaseHistoryEvent::Reclaimed { .. } => "Reclaimed",
        LeaseHistoryEvent::Revoked { .. } => "Revoked",
        _ => "Other",
    }
}

// ─── Worker loop: drive executions to terminal ───────────────────

async fn run_worker_loop(
    worker: FlowFabricWorker,
    admin: Arc<FlowFabricAdminClient>,
    done: Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let lane = LaneId::try_new(LANE)?;
    info!("worker loop started");
    loop {
        tokio::select! {
            _ = done.notified() => {
                info!("worker shutdown");
                return Ok(());
            }
            claim = worker.claim_via_server(admin.as_ref(), &lane, 1_000) => {
                match claim {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().to_string();
                        let attempt = task.attempt_index().0;
                        info!(execution_id = %eid, attempt, "claimed task");
                        task.complete(Some(b"{\"ok\":true}".to_vec())).await?;
                        info!(execution_id = %eid, "completed successfully");
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                    Err(e) => {
                        warn!(error = %e, "claim failed");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }
}

// ─── HTTP helpers ────────────────────────────────────────────────

async fn submit_tagged(
    http: &reqwest::Client,
    server_url: &str,
    partition_config: &PartitionConfig,
    tenant: &str,
) -> Result<ExecutionId, Box<dyn std::error::Error + Send + Sync>> {
    let lane = LaneId::try_new(LANE)?;
    let eid = ExecutionId::solo(&lane, partition_config);
    let payload = NoopPayload {
        tenant: tenant.to_string(),
    };
    let policy = ExecutionPolicy::default();
    let partition_id: u16 = eid.partition();
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": NAMESPACE,
        "lane_id": LANE,
        "execution_kind": "v010_read_side_demo",
        "input_payload": serde_json::to_vec(&payload)?,
        "payload_encoding": "json",
        "priority": 0,
        "creator_identity": "v010-read-side-ergonomics-example",
        "idempotency_key": serde_json::Value::Null,
        "tags": { TAG_KEY: tenant },
        "policy": serde_json::to_value(&policy)?,
        "delay_until": serde_json::Value::Null,
        "partition_id": partition_id,
        "now": now_ms(),
    });
    let resp = http
        .post(format!("{server_url}/v1/executions"))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("create_execution failed: {status} {text}").into());
    }
    Ok(eid)
}

async fn poll_until_terminal(
    http: &reqwest::Client,
    server_url: &str,
    execution_id: &ExecutionId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{server_url}/v1/executions/{execution_id}/state");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("timed out waiting for {execution_id} to reach terminal").into());
        }
        let resp = http.get(&url).send().await?;
        if resp.status().is_success() {
            // `GET /v1/executions/{id}/state` returns a bare JSON string,
            // e.g. `"completed"`. Match the retry-and-cancel example's
            // terminal-set: the public-state taxonomy is defined by
            // `ff_core::state::PublicState`.
            let state: String = resp
                .json::<serde_json::Value>()
                .await?
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            if matches!(
                state.as_str(),
                "completed" | "failed" | "cancelled" | "expired" | "skipped"
            ) {
                info!(execution_id = %execution_id, %state, "terminal");
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
