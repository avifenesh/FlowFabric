//! Spawn an in-process `ff_server::Server` (tests 1–5) or the
//! `ff-server` binary (test 6) against the test Valkey.
//!
//! # Binary spawner
//!
//! Uses `CARGO_BIN_EXE_ff-server` so integration tests depend on a
//! fresh build. `FF_LISTEN_ADDR` accepts an ephemeral port such as
//! `127.0.0.1:0` (the server binds via `TcpListener::bind`, same as
//! the in-process path here). The process is guarded via `ChildGuard`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

use ff_core::types::LaneId;
use ff_server::config::ServerConfig;
use ff_server::server::Server;

use crate::valkey::TEST_PARTITION_CONFIG;

/// In-process server for tests 1–5. Wraps `Server::start` + an axum
/// listener on a random port, same pattern as `ff-test::e2e_api`.
///
/// Tests MUST call [`InProcessServer::shutdown`] before the binding
/// goes out of scope. `Drop` only aborts the axum listener as a
/// best-effort fallback — it cannot drain the engine's
/// `completion_listener` / background tasks, which leak across tests
/// in the same process and corrupt Valkey state for subsequent tests.
pub struct InProcessServer {
    pub server: Arc<Server>,
    pub base_url: String,
    pub axum_handle: JoinHandle<()>,
}

impl Drop for InProcessServer {
    fn drop(&mut self) {
        // Best-effort fallback only — async cleanup (engine drain,
        // background task join) is impossible from `Drop`. If a test
        // panics before reaching `.shutdown().await` this at least
        // stops new axum connections. The engine's background tasks
        // will still leak until the process exits.
        tracing::warn!(
            "InProcessServer dropped without explicit `.shutdown().await` — \
             engine completion_listener and background tasks may leak across \
             tests; call `server.shutdown().await` at the end of the test"
        );
        self.axum_handle.abort();
    }
}

impl InProcessServer {
    pub async fn start(lane: &str) -> Self {
        Self::start_with_secret(
            lane,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await
    }

    /// Boot an in-process server using `secret` as the waitpoint HMAC
    /// key, bypassing `ServerConfig::from_env` (the config is assembled
    /// directly and `Server::start` does not re-validate hex shape).
    /// Callers are responsible for supplying a secret that matches the
    /// `from_env` contract (even-length ASCII hex, non-empty) when they
    /// want to exercise that contract. Used by the README-literal
    /// readiness test to cover the quickstart secret shape (2N hex
    /// chars from `openssl rand -hex N`).
    pub async fn start_with_secret(lane: &str, secret: &str) -> Self {
        let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
        let port: u16 = std::env::var("FF_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6379);
        let tls = crate::valkey::env_flag("FF_TLS");
        let cluster = crate::valkey::env_flag("FF_CLUSTER");

        let config = ServerConfig {
            host,
            port,
            tls,
            cluster,
            partition_config: TEST_PARTITION_CONFIG,
            lanes: vec![LaneId::new(lane)],
            listen_addr: "127.0.0.1:0".into(),
            engine_config: ff_engine::EngineConfig {
                partition_config: TEST_PARTITION_CONFIG,
                lanes: vec![LaneId::new(lane)],
                ..Default::default()
            },
            skip_library_load: true,
            cors_origins: vec!["*".to_owned()],
            api_token: None,
            waitpoint_hmac_secret: secret.to_owned(),
            waitpoint_hmac_grace_ms: 86_400_000,
            max_concurrent_stream_ops: 64,
        };

        let server = Arc::new(Server::start(config).await.expect("Server::start"));
        let app = ff_server::api::router(server.clone(), &["*".to_owned()], None)
            .expect("router with wildcard CORS");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr: SocketAddr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let axum_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Wait for /healthz to return 200 before handing back. The
        // per-request timeout keeps a stalled connect from outlasting
        // the outer 5s deadline (each probe is capped at 500ms).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("build healthz probe client");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let url = format!("{base_url}/healthz");
        loop {
            if let Ok(r) = client.get(&url).send().await
                && r.status().is_success()
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("ff-server in-process /healthz never returned 200");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            server,
            base_url,
            axum_handle,
        }
    }

    /// Required teardown for `InProcessServer`. Drains background tasks
    /// and shuts down the engine.
    ///
    /// `Drop` is only a best-effort fallback that emits a warning —
    /// always call this explicitly at the end of every test.
    ///
    /// Behavior:
    /// 1. Abort the axum listener task and await its `JoinHandle` so
    ///    no new HTTP requests can observe a half-shut-down engine.
    /// 2. Unwrap the `Arc<Server>` — panics with an actionable message
    ///    if the caller still holds clones of `server`.
    /// 3. Call `Server::shutdown`, which closes the stream semaphore,
    ///    drains background tasks (15s ceiling) and shuts down the
    ///    engine. The step is wrapped in a 1s defensive outer
    ///    timeout; a timeout panics with an actionable message.
    pub async fn shutdown(self) {
        // The struct implements `Drop`, so fields cannot be moved out
        // directly. Wrap in `ManuallyDrop` to suppress the
        // warn-emitting fallback drop, then read each field out by
        // value exactly once.
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is a fresh `ManuallyDrop` wrapper; each field
        // is read exactly once and never accessed again through
        // `this`. `ManuallyDrop` suppresses `Drop::drop`, so nothing
        // is double-dropped; each moved value is dropped on its own
        // path below (or at this function's end).
        let axum_handle = unsafe { std::ptr::read(&this.axum_handle) };
        let server_arc = unsafe { std::ptr::read(&this.server) };
        let _base_url = unsafe { std::ptr::read(&this.base_url) };

        // Stop accepting new HTTP connections.
        axum_handle.abort();
        // Await so the task is fully torn down before we touch the
        // shared `Arc<Server>` — `JoinError::cancelled` is expected.
        let _ = axum_handle.await;

        let server = Arc::try_unwrap(server_arc).unwrap_or_else(|_arc| {
            panic!(
                "InProcessServer::shutdown: outside clones of `server` still exist; \
                 drop all clones before calling `.shutdown().await`"
            )
        });

        // Defensive outer timeout. `Server::shutdown` bounds its own
        // drain at ~15s internally, but readiness tests have no
        // expected long-running background work at teardown — anything
        // past 1s indicates a stuck engine or task.
        match tokio::time::timeout(Duration::from_secs(1), server.shutdown()).await {
            Ok(()) => {}
            Err(_) => panic!(
                "InProcessServer::shutdown: Server::shutdown did not complete within \
                 1s — engine or background tasks are stuck; inspect tracing logs"
            ),
        }
    }
}
