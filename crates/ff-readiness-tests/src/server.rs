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
use tokio::task::AbortHandle;

use ff_core::types::LaneId;
use ff_server::config::ServerConfig;
use ff_server::server::Server;

use crate::valkey::TEST_PARTITION_CONFIG;

/// In-process server for tests 1–5. Wraps `Server::start` + an axum
/// listener on a random port, same pattern as `ff-test::e2e_api`.
pub struct InProcessServer {
    pub server: Arc<Server>,
    pub base_url: String,
    pub abort: AbortHandle,
}

impl Drop for InProcessServer {
    fn drop(&mut self) {
        self.abort.abort();
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
    /// key (validated by `ServerConfig::from_env`'s rules: even-length
    /// ASCII hex). Used by the README-literal readiness test to prove
    /// the boot contract the quickstart documents.
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

        let handle = tokio::spawn(async move {
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
            abort: handle.abort_handle(),
        }
    }
}
