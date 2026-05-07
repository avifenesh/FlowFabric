//! v016-worker-runtime — handler-DI demo for issue #331.
//!
//! Problem statement (from the apalis comparison, see
//! `benches/results/COMPARISON.md`): every FlowFabric consumer was
//! rebuilding the same four pieces of plumbing around
//! `claim_next_via_backend`:
//!
//! - idle-backoff between empty claims
//! - a bounded tokio::spawn fan-out for parallelism
//! - panic catch so a single bad task doesn't kill the worker loop
//! - shared-state threading (HTTP client, DB pool, config) to handler fns
//!
//! `ff_sdk::runtime::WorkerRuntime` wraps a [`FlowFabricWorker`] and
//! supplies all four. Handlers are plain async fns whose arguments
//! implement `FromTask` — [`Payload<P>`] for JSON input,
//! [`Data<T>`] for typed shared state, [`TaskInfo`] for immutable
//! task metadata.
//!
//! # What this binary does
//!
//! It connects the worker, registers two handlers + one shared state
//! value, builds a [`WorkerRuntime`] and drives it for 5s against the
//! configured Valkey (or until SIGTERM). No executions are seeded
//! from inside this demo — point a producer (e.g. the `ff-server`
//! admin API or a cairn-style control-plane) at the same Valkey and
//! submit work under `execution_kind = "enrich"` or `"bad"` to
//! observe the runtime in action.
//!
//! The demo succeeds by **compiling, connecting, and running idle
//! without panicking**. That tight scope is intentional — wiring a
//! producer inline would turn a ~100 LOC ergonomics demo into a
//! multi-crate smoke harness. The headline example for the v0.16
//! release will include end-to-end seeding; this file is the
//! compile-surface demo that ships alongside the feature.
//!
//! # Prereqs
//!
//! * Valkey 8.x on 127.0.0.1:6379 (or override via `FF_HOST` / `FF_PORT`).
//!
//! Run:
//!   FF_HOST=127.0.0.1 FF_PORT=6379 cargo run --bin v016-worker-runtime

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use ff_core::backend::BackendConfig;
use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};
use ff_sdk::config::WorkerConfig;
use ff_sdk::runtime::{Data, HandlerResult, Payload, TaskInfo, WorkerRuntime};
use ff_sdk::FlowFabricWorker;

/// Mock "HTTP client" — just tracks call counts. Real handlers would
/// inject a `reqwest::Client` or `sqlx::PgPool` here.
#[derive(Default)]
struct HttpClient {
    calls: AtomicU64,
}

#[derive(Debug, Deserialize)]
struct EnrichJob {
    url: String,
}

#[derive(Debug, Serialize)]
struct EnrichResult {
    url: String,
    status: u16,
    attempt: u32,
}

#[derive(Debug, Deserialize)]
struct BadJob {
    #[serde(rename = "fail_with")]
    reason: String,
}

#[derive(Debug, thiserror::Error)]
#[error("simulated failure: {0}")]
struct SimulatedFail(String);

/// Handler for the "enrich" kind. Returns a completion payload.
async fn enrich(
    Payload(job): Payload<EnrichJob>,
    Data(http): Data<Arc<HttpClient>>,
    info: TaskInfo,
) -> HandlerResult {
    let n = http.calls.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(
        exec = %info.execution_id,
        attempt = %info.attempt_index,
        url = %job.url,
        "enrich handler invoked"
    );
    let payload = EnrichResult {
        url: job.url,
        status: 200,
        attempt: info.attempt_index.0,
    };
    let bytes = serde_json::to_vec(&payload)?;
    // Tiny synthetic delay so the concurrency cap is observable.
    tokio::time::sleep(Duration::from_millis(20)).await;
    tracing::info!(%n, "enrich completed");
    Ok(Some(bytes))
}

/// Handler for the "bad" kind. Always returns `Err(..)` so the runtime
/// routes through the fail-path with `error_category="handler_error"`.
async fn bad(Payload(job): Payload<BadJob>) -> HandlerResult {
    Err(Box::new(SimulatedFail(job.reason)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "v016_worker_runtime=info,ff_sdk=info".into()),
        )
        .init();

    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);

    tracing::info!(%host, port, "connecting");

    let worker_config = WorkerConfig {
        backend: Some(BackendConfig::valkey(host.clone(), port)),
        worker_id: WorkerId::new("v016-runtime-demo"),
        worker_instance_id: WorkerInstanceId::new(format!(
            "v016-runtime-demo-{}",
            std::process::id()
        )),
        namespace: Namespace::new("v016-runtime-demo"),
        lanes: vec![LaneId::new("v016-demo")],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
        partition_config: None,
    };
    let worker = FlowFabricWorker::connect(worker_config)
        .await
        .context("FlowFabricWorker::connect")?;

    let http = Arc::new(HttpClient::default());
    let http_for_summary = http.clone();
    let runtime = WorkerRuntime::new(worker)
        .data(http)
        .on("enrich", enrich)
        .on("bad", bad)
        .max_concurrent(4);

    tracing::info!("runtime driving for 5s (point a producer at Valkey to exercise handlers) …");
    let run = runtime.run();
    let _ = timeout(Duration::from_secs(5), run).await;

    tracing::info!(
        enrich_calls = http_for_summary.calls.load(Ordering::Relaxed),
        "demo complete — v016-worker-runtime"
    );
    Ok(())
}
