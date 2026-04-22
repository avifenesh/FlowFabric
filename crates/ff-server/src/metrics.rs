//! PR-94: Prometheus /metrics endpoint + HTTP metrics middleware.
//!
//! This module is a thin wrapper around [`ff_observability::Metrics`].
//! It compiles in both feature configurations:
//!
//! * `observability` **off** — [`Metrics`] re-exports the no-op shim
//!   so call sites in `api::router` use an identical signature. The
//!   `/metrics` route is not mounted (see `api::router`) so serve is
//!   404; the HTTP middleware is not installed.
//! * `observability` **on** — real OTEL-backed registry, `/metrics`
//!   mounted, HTTP middleware installed that records
//!   `ff_http_requests_total` + `ff_http_request_duration_seconds`
//!   labelled by `method` + `path` (`MatchedPath`, so parameterized
//!   paths like `/v1/executions/{id}` collapse to one series) +
//!   `status`.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};

pub use ff_observability::Metrics;

/// GET /metrics — Prometheus text exposition (`text/plain; version=0.0.4`).
///
/// # Authentication
///
/// Intentionally unauthenticated. Matches Prometheus operational
/// convention: network-layer (ingress ACL, service-mesh policy, or
/// cluster-internal-only listen) gates scrape access. FlowFabric does
/// not own auth for scrape endpoints.
///
/// If you need to restrict scrapers, constrain the listen address
/// (bind to the metrics-only interface) or set ingress rules.
#[cfg_attr(not(feature = "observability"), allow(dead_code))]
pub async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> Response {
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

/// Axum middleware: record HTTP method + `MatchedPath` + status + duration.
///
/// Runs after the handler finishes so we see the final status. Missing
/// `MatchedPath` (404 for unrouted paths) is labelled `"unknown"` to
/// cap cardinality — a flood of distinct 404 paths would otherwise
/// explode the `path` label space.
#[cfg_attr(not(feature = "observability"), allow(dead_code))]
pub async fn http_middleware(
    State(metrics): State<Arc<Metrics>>,
    req: Request,
    next: middleware::Next,
) -> Response {
    let method = req.method().as_str().to_owned();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let start = Instant::now();
    let resp = next.run(req).await;
    let elapsed = start.elapsed();
    let status = resp.status().as_u16();
    metrics.record_http_request(&method, &path, status, elapsed);
    resp
}
