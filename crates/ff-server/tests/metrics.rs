//! PR-94: /metrics endpoint + text-exposition format integration test.
//!
//! Feature-gated (`observability` off → test body is `fn () {}`, stays
//! compiled but exercises nothing). The full scope runs under the
//! dedicated CI job (`cargo test -p ff-server --features observability`)
//! added alongside this PR.

#![cfg(feature = "observability")]

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, http::Request, routing::get};
use ff_observability::Metrics;
use ff_server::metrics;
use tower::ServiceExt;

/// Minimal router that mounts `/metrics` + the HTTP middleware
/// against a noop handler. Mirrors the shape `api::router_with_metrics`
/// assembles without needing a live Valkey / `Server`.
fn test_router(m: Arc<Metrics>) -> Router {
    let noop = Router::new()
        .route("/noop", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            m.clone(),
            metrics::http_middleware,
        ))
        .with_state(());

    let metrics_router = Router::new()
        .route("/metrics", get(metrics::metrics_handler))
        .with_state(m);

    noop.merge(metrics_router)
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text_format() {
    let m = Arc::new(Metrics::new());
    let app = test_router(m.clone());

    // Trigger one HTTP request so the counter has a non-zero sample.
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/noop").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Also exercise the non-HTTP instruments so `/metrics` contains
    // non-zero samples for scanner / claim / lease / etc. counters.
    m.record_scanner_cycle("test_scanner", Duration::from_millis(25));
    m.record_claim_from_grant("default", Duration::from_millis(5));
    m.inc_lease_renewal("ok");
    m.inc_worker_at_capacity();
    m.inc_budget_hit("tokens");
    m.inc_quota_hit("rate");
    m.set_cancel_backlog_depth(3);

    let resp = app
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .expect("content-type present")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        ct.starts_with("text/plain"),
        "content-type should be text/plain for Prometheus text format; got {ct}"
    );

    let body_bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body read");
    let body = std::str::from_utf8(&body_bytes).expect("utf-8 body");

    // Every metric from the PR-94 inventory appears in the exposition.
    for name in [
        "ff_http_requests_total",
        "ff_http_request_duration_seconds",
        "ff_scanner_cycle_duration_seconds",
        "ff_scanner_cycle_total",
        "ff_cancel_backlog_depth",
        "ff_claim_from_grant_duration_seconds",
        "ff_lease_renewal_total",
        "ff_worker_at_capacity_total",
        "ff_budget_hit_total",
        "ff_quota_hit_total",
    ] {
        assert!(
            body.contains(name),
            "/metrics output missing `{name}`. Full body:\n{body}"
        );
    }

    // Counter samples we triggered above should appear as non-zero.
    // `http_requests_total` above /noop is the easiest load-bearing
    // check — if the middleware didn't fire, this line is missing.
    assert!(
        body.lines().any(|l| l.starts_with("ff_http_requests_total{")),
        "ff_http_requests_total has no label-set samples. Body:\n{body}"
    );
    assert!(
        body.lines().any(|l| {
            l.starts_with("ff_cancel_backlog_depth") && l.trim_end().ends_with(" 3")
        }),
        "cancel_backlog_depth gauge should reflect set(3). Body:\n{body}"
    );
    assert!(
        body.lines()
            .any(|l| l.contains("ff_lease_renewal_total") && l.contains("outcome=\"ok\"")),
        "lease_renewal counter missing outcome=ok sample. Body:\n{body}"
    );
}

#[tokio::test]
async fn metrics_endpoint_unauthenticated() {
    // Scrape is intentionally not gated by FF_API_TOKEN (auth is a
    // network-layer concern). This test exercises the shape — the
    // handler does not consult any auth header; if a future refactor
    // accidentally wires auth in, this test fails before ops sees it.
    let m = Arc::new(Metrics::new());
    let app = test_router(m);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                // Deliberately no Authorization header.
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
