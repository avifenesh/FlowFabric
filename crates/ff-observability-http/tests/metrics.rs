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

use ff_observability::Metrics;
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
