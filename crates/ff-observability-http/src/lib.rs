//! FlowFabric consumer-side Prometheus `/metrics` HTTP endpoint.
//!
//! This crate bridges the [`ff_observability::Metrics`] registry (the
//! single owner of the OTEL + Prometheus dep surface) to an axum
//! [`Router`] so consumers embedding `ff-sdk` in library mode can expose
//! a scrape endpoint without also pulling `ff-server`.
//!
//! `ff-server` has its own `/metrics` wiring (see `ff-server::metrics`
//! PR #94); this crate is for the consumer-in-library-mode use case
//! where no ff-server process exists.
//!
//! ## Shape
//!
//! Two entry points, both thin:
//!
//! * [`router`] — returns an `axum::Router` the consumer merges into
//!   their own axum app. Mounts `GET /metrics`.
//! * [`serve`] — convenience: binds the address and runs the router
//!   on the current tokio runtime. Use only when you don't already
//!   have an axum app.
//!
//! ## Feature model
//!
//! Matches `ff-observability`. The `enabled` feature on this crate
//! transitively enables `ff-observability/enabled`, so a consumer
//! only needs one feature flag in their own `Cargo.toml`:
//!
//! ```toml
//! ff-observability-http = { version = "0.4", features = ["enabled"] }
//! ```
//!
//! With the feature OFF, [`Metrics::render`](ff_observability::Metrics::render)
//! returns an empty string and `/metrics` responds 200 with an empty
//! body — the consumer can still wire the route unconditionally.
//!
//! ## Authentication
//!
//! The endpoint is intentionally unauthenticated, matching Prometheus
//! operational convention: network-layer (ingress ACL, service-mesh
//! policy, or interface-bind) gates scrape access. If you need auth,
//! mount [`router`] behind your own middleware stack.

#![forbid(unsafe_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use ff_observability::Metrics;

/// Build an axum [`Router`] with `GET /metrics` mounted.
///
/// The returned router owns an `Arc<Metrics>` so it is cheap to clone
/// and safe to `merge` into a larger app. The handler responds with
/// Prometheus text exposition format (`text/plain; version=0.0.4`).
pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics)
}

/// Bind `addr` and serve the metrics router until the task is cancelled.
///
/// Convenience wrapper for consumers that don't already have an axum
/// app. Uses [`tokio::net::TcpListener`] + [`axum::serve`] directly so
/// there is no extra runtime configuration to think about.
///
/// Returns an [`io::Error`] if the bind fails. Runtime errors from
/// `axum::serve` surface through the same error type.
pub async fn serve(addr: SocketAddr, metrics: Arc<Metrics>) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(metrics)).await
}

/// GET /metrics handler. Returns Prometheus text exposition.
async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> Response {
    let body = metrics.render();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}
