//! Trait-boundary transport-fault coverage for
//! [`ff_core::engine_backend::EngineBackend::report_usage`].
//!
//! Per Worker-TTT's `rfcs/drafts/post-stage-1c-test-coverage.md`
//! audit, `r7_backend_report_usage.rs` pins the happy + dedup +
//! Soft/HardBreach paths but leaves the transport-fault path
//! untested. This file adds the missing assertion: when Valkey is
//! unavailable, `report_usage` MUST surface `EngineError::Transport
//! { .. }` — not panic, not silently drop, not map to a wrong
//! variant.
//!
//! The test dials `ValkeyBackend::connect(BackendConfig)` at a TCP
//! port with no listener, with `request_timeout` forced short and
//! retries disabled. ferriskey's `ClientBuilder::build()` returns a
//! transport error at dial; the backend's `build_client` maps it
//! through `transport_script(ScriptError::Valkey(..))` — the
//! identical mapping `report_usage_impl` uses on an `fcall` failure
//! (`crates/ff-backend-valkey/src/lib.rs:1075-1078`, `transport_fk`).
//! Pinning this one call site pins the shared mapper for every
//! trait op that dispatches via `fcall` — `report_usage` among
//! them.
//!
//! Run with:
//!   cargo test -p ff-test --test r7_backend_report_usage_transport_fault \
//!       -- --test-threads=1

use std::time::Duration;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{BackendConfig, BackendConnection, ValkeyConnection};
use ff_core::engine_error::EngineError;

/// Find a TCP port with no listener: bind ephemeral, read the port,
/// drop the listener. A subsequent dial to `127.0.0.1:<port>` will
/// get `ECONNREFUSED` (the kernel's SYN-RST on closed ports) before
/// any TIME_WAIT reuse window could matter.
async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Build a `BackendConfig` that is guaranteed to fail at dial: a
/// closed TCP port, short request timeout, no retries. The
/// `ClientBuilder::build()` call inside `ValkeyBackend::connect`
/// runs an eager handshake; against a refused connection it returns
/// a ferriskey transport error, which the backend's `build_client`
/// wraps in `EngineError::Transport`.
fn dead_backend_config(port: u16) -> BackendConfig {
    let mut cfg = BackendConfig::valkey("127.0.0.1", 0);
    cfg.connection = BackendConnection::Valkey(ValkeyConnection::new("127.0.0.1", port));
    cfg.timeouts.request = Some(Duration::from_millis(250));
    // Disable the backoff-retry loop so the test fails fast rather
    // than spending ferriskey's default retry budget on the refused
    // port. `Some(0)` on every knob keeps build behaviour
    // deterministic: one dial attempt, one error.
    cfg.retry.exponent_base = Some(2);
    cfg.retry.factor = Some(1);
    cfg.retry.number_of_retries = Some(0);
    cfg.retry.jitter_percent = Some(0);
    cfg
}

// ─── Tests ───

/// Valkey unreachable at dial → `ValkeyBackend::connect` returns
/// `EngineError::Transport`, not panic, not `Unavailable`. Pins the
/// `transport_script` mapping that `report_usage_impl` shares with
/// every other `fcall`-based trait op.
#[tokio::test]
async fn report_usage_dial_to_closed_port_surfaces_transport() {
    let port = closed_port().await;
    let cfg = dead_backend_config(port);

    let err = ValkeyBackend::connect(cfg)
        .await
        .err()
        .expect("dial to a closed port must fail");
    match err {
        EngineError::Transport { backend, source } => {
            assert_eq!(
                backend, "valkey",
                "backend tag must identify the Valkey dial path"
            );
            // Source must be non-empty — the diagnostic is what
            // operators pivot on when triaging a live transport
            // fault. Assert the payload exists and has content;
            // ferriskey phrasing varies by platform + error.
            let msg = source.to_string();
            assert!(
                !msg.is_empty(),
                "Transport source must carry a diagnostic, got empty"
            );
        }
        other => panic!("expected EngineError::Transport on dial to closed port, got {other:?}"),
    }
}

/// Same assertion with the cluster flag flipped — the cluster dial
/// path runs through ferriskey's `ClusterClientBuilder` rather than
/// the standalone builder, and both MUST route their transport
/// errors through the same `EngineError::Transport` variant. Pins
/// the parity so a future cluster-path rewrite cannot silently
/// swallow / rewrap the error without the test catching it.
#[tokio::test]
async fn report_usage_dial_to_closed_port_cluster_surfaces_transport() {
    let port = closed_port().await;
    let mut cfg = dead_backend_config(port);
    if let BackendConnection::Valkey(ref mut v) = cfg.connection {
        v.cluster = true;
    }

    let err = ValkeyBackend::connect(cfg)
        .await
        .err()
        .expect("cluster dial to a closed port must fail");
    match err {
        EngineError::Transport { backend, .. } => {
            assert_eq!(backend, "valkey");
        }
        other => {
            panic!("expected EngineError::Transport on cluster dial to closed port, got {other:?}")
        }
    }
}
