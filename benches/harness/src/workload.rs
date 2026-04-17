//! Workload primitives shared across scenarios.
//!
//! The harness assumes a running `ff-server` on `FF_BENCH_SERVER`
//! (default `http://localhost:9090`) backed by Valkey on
//! `FF_BENCH_VALKEY_HOST`:`FF_BENCH_VALKEY_PORT` (default
//! `localhost:6379`). Spin-up is the operator's responsibility —
//! `benches/README.md` documents the one-liner.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

/// Tunables resolved at scenario entry. CLI can override; env falls back
/// to defaults for `cargo bench` invocations.
#[derive(Clone, Debug)]
pub struct BenchEnv {
    pub server: String,
    pub valkey_host: String,
    pub valkey_port: u16,
    pub namespace: String,
    pub lane: String,
    pub cluster: bool,
}

impl BenchEnv {
    pub fn from_env() -> Self {
        fn var(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_owned())
        }
        Self {
            server: var("FF_BENCH_SERVER", "http://localhost:9090"),
            valkey_host: var("FF_BENCH_VALKEY_HOST", "localhost"),
            valkey_port: var("FF_BENCH_VALKEY_PORT", "6379")
                .parse()
                .unwrap_or(6379),
            namespace: var("FF_BENCH_NAMESPACE", "default"),
            lane: var("FF_BENCH_LANE", "bench"),
            cluster: var("FF_BENCH_CLUSTER", "false") == "true",
        }
    }
}

/// Minimal HTTP client pre-seeded with the bearer token if one is
/// configured. Timeouts are generous — benches hammer the server and
/// small spikes shouldn't fail a run.
pub fn http_client() -> Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    if let Ok(token) = std::env::var("FF_API_TOKEN") {
        if !token.is_empty() {
            let mut h = reqwest::header::HeaderMap::new();
            let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("bad FF_API_TOKEN")?;
            h.insert(reqwest::header::AUTHORIZATION, val);
            builder = builder.default_headers(h);
        }
    }
    Ok(builder.build()?)
}

/// POST /v1/executions with no required capabilities. Returns the
/// execution_id so the caller can correlate on completion.
pub async fn create_execution(
    client: &Client,
    env: &BenchEnv,
    kind: &str,
    payload_bytes: Vec<u8>,
) -> Result<String> {
    let eid = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": env.namespace,
        "lane_id": env.lane,
        "execution_kind": kind,
        "input_payload": payload_bytes,
        "payload_encoding": "binary",
        "priority": 100,
        "creator_identity": "ff-bench",
        "tags": {},
        "partition_id": 0,
        "now": now_ms(),
    });
    let url = format!("{}/v1/executions", env.server);
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("create_execution failed ({status}): {text}");
    }
    Ok(eid)
}

/// Poll /v1/executions/{id}/state until a terminal value appears.
/// Returns the terminal state.
pub async fn wait_terminal(
    client: &Client,
    env: &BenchEnv,
    eid: &str,
    poll_interval: Duration,
    deadline: Duration,
) -> Result<String> {
    let url = format!("{}/v1/executions/{eid}/state", env.server);
    let start = std::time::Instant::now();
    loop {
        let resp = client.get(&url).send().await?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await?;
            let s = v.as_str().unwrap_or("unknown").to_owned();
            if matches!(
                s.as_str(),
                "completed" | "failed" | "cancelled" | "expired"
            ) {
                return Ok(s);
            }
        }
        if start.elapsed() > deadline {
            anyhow::bail!("wait_terminal {eid}: deadline exceeded");
        }
        tokio::time::sleep(poll_interval).await;
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Helper for scenarios that want a fixed-size random payload. Uses
/// nanos-of-system-time as a cheap entropy source — benches don't need
/// crypto-grade randomness.
pub fn filler_payload(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E3779B9);
    for _ in 0..size {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        buf.push(((x >> 16) & 0xFF) as u8);
    }
    buf
}
