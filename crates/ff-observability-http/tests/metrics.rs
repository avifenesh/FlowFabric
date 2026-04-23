//! Integration test: spin the `/metrics` endpoint and hit it.
//!
//! Runs only when the `enabled` feature is on — that's the only
//! configuration where `Metrics::render()` produces non-empty output,
//! which is what the assertion checks for. Under the default (disabled)
//! build the test is compiled out so the crate's own CI-default matrix
//! stays clean.

#![cfg(feature = "enabled")]

use std::sync::Arc;
use std::time::Duration;

use ff_observability::{AttemptOutcome, Metrics};
use ff_observability_http::router;

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let metrics = Arc::new(Metrics::new());
    // Emit at least one sample so the exposition body is non-empty.
    metrics.inc_lease_renewal("ok");

    // Bind to an ephemeral port, capture the actual addr, then serve.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(Arc::clone(&metrics));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Minimal HTTP/1.1 GET — avoids adding `reqwest` / `hyper-util`
    // just for this one probe. We only need status + body substring.
    let resp = raw_get(addr, "/metrics").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "status line: {resp:?}");
    assert!(
        resp.contains("ff_lease_renewal_total")
            || resp.contains("ff_lease_renewal"),
        "expected a metric line in body, got: {resp}"
    );

    server.abort();
    let _ = server.await;
}

/// Scrape `/metrics` after firing a mix of attempt outcomes across
/// two lanes and assert the counter surfaces with both labels.
#[tokio::test]
async fn attempt_outcome_counter_scrapes_with_lane_and_outcome() {
    let metrics = Arc::new(Metrics::new());
    metrics.inc_attempt_outcome("default", AttemptOutcome::Ok);
    metrics.inc_attempt_outcome("default", AttemptOutcome::Ok);
    metrics.inc_attempt_outcome("default", AttemptOutcome::Error);
    metrics.inc_attempt_outcome("priority", AttemptOutcome::Timeout);
    metrics.inc_attempt_outcome("priority", AttemptOutcome::Cancelled);
    metrics.inc_attempt_outcome("priority", AttemptOutcome::Retry);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(Arc::clone(&metrics));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let resp = raw_get(addr, "/metrics").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "status line: {resp:?}");
    assert!(
        resp.contains("ff_attempt_outcome_total"),
        "counter name missing: {resp}"
    );
    // Each of the 5 outcome labels must appear at least once, and
    // both lane labels must be present.
    for outcome in ["ok", "error", "timeout", "cancelled", "retry"] {
        let needle = format!("outcome=\"{outcome}\"");
        assert!(resp.contains(&needle), "missing {needle} in: {resp}");
    }
    assert!(resp.contains("lane=\"default\""), "missing lane=default");
    assert!(resp.contains("lane=\"priority\""), "missing lane=priority");
    // Double-count check: the (default, ok) series fired twice and must
    // be exposed as value 2 (line ends with ` 2`).
    let ok_line = resp
        .lines()
        .find(|l| {
            l.starts_with("ff_attempt_outcome_total{")
                && l.contains("lane=\"default\"")
                && l.contains("outcome=\"ok\"")
        })
        .unwrap_or_else(|| panic!("no (default, ok) series line in: {resp}"));
    assert!(
        ok_line.trim_end().ends_with(" 2"),
        "expected value=2 on {ok_line}"
    );

    server.abort();
    let _ = server.await;
}

async fn raw_get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::with_capacity(8192);
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("response within 5s")
        .unwrap();
    assert!(read > 0, "empty response");
    String::from_utf8_lossy(&buf).into_owned()
}
