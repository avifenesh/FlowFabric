//! Round-2 cluster+TLS bench helpers.
//!
//! Sits alongside `shared.rs`. The 12 round-2 bins each pull this in
//! via `#[path = "../cluster_shared.rs"] mod cluster_shared;` (same
//! shape as round 1's `shared.rs`). The reason it's its own module
//! and not grafted onto `shared.rs` is `tls` + `cluster`-mode client
//! construction is specific to the cluster bins — pulling it into the
//! round-1 helpers would needlessly bring cluster-redis features into
//! every round-1 build path and blur the investigation scope.
//!
//! Key-hygiene rule for every cluster bin: ONE hash tag, ONE slot.
//! Round 2 is about client-in-cluster-mode + TLS overhead, NOT
//! cross-slot routing. Any cluster bin that writes a key without a
//! `{wider}` or `{benchq}` tag is a measurement bug; a post-run
//! DBSIZE check flags leakage.

use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};
use ferriskey::{Client as FkClient, ClientBuilder as FkBuilder};

/// Install the process-wide rustls `CryptoProvider` exactly once.
///
/// rustls 0.23 refuses to hand out a default provider unless exactly
/// one feature is enabled. Ferriskey pins `aws-lc-rs` internally; the
/// redis crate's `tls-rustls` feature set does NOT pick a provider,
/// so without this call `redis::ClusterClient::get_async_connection`
/// panics at first TLS handshake. Matching ferriskey's choice keeps
/// the TLS primitives identical across both clients in round-2
/// benches — a fair A/B.
///
/// `Once` guards the install so calling this from every bin's main
/// (and from every worker that might initialise TLS later) is safe.
pub fn init_rustls_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Connection config read from env — matches
/// `crates/ff-test/src/fixtures.rs::build_client_from_env` so a
/// cluster-aware bench bin takes the same knobs operators already know.
pub struct ClusterEnv {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub cluster: bool,
}

impl ClusterEnv {
    pub fn from_env() -> Self {
        Self {
            host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
            port: std::env::var("FF_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6379),
            tls: env_flag("FF_TLS"),
            cluster: env_flag("FF_CLUSTER"),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "host={} port={} tls={} cluster={}",
            self.host, self.port, self.tls, self.cluster
        )
    }
}

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

/// Build a ferriskey Client using the project's canonical pattern
/// (`ClientBuilder.host().tls().cluster().build()` — same as
/// fixtures.rs). No URL parsing. TLS + cluster only flip on when the
/// env flag is set, which matches how FF_HOST / FF_TLS / FF_CLUSTER
/// operate everywhere else in the project.
#[allow(dead_code)] // only used by cluster_ferriskey_* bins
pub async fn build_ferriskey(env: &ClusterEnv) -> Result<FkClient> {
    let mut builder = FkBuilder::new()
        .host(&env.host, env.port)
        .connect_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_millis(5_000));
    if env.tls {
        builder = builder.tls();
    }
    if env.cluster {
        builder = builder.cluster();
    }
    builder.build().await.context("ferriskey ClientBuilder.build()")
}

/// Build a redis-rs `ClusterClient` with matching TLS + cluster
/// semantics. Returns an `Arc` since `get_async_connection` takes
/// `&self` and we need to share one ClusterClient across worker
/// tasks to pool connections correctly (each worker gets its OWN
/// async connection via `get_async_connection().await`).
///
/// Strict TLS verification. The redis crate ships `tls-rustls-
/// webpki-roots` in our feature set, which bakes the Mozilla trust
/// store into the binary — reproducible on any host regardless of
/// /etc/ssl state. `TlsMode::Secure` enables full cert chain +
/// hostname verification. A verify failure here is a finding, NOT
/// a fallback-to-insecure signal.
#[allow(dead_code)] // only used by cluster_redis_* bins
pub fn build_redis_cluster(env: &ClusterEnv) -> Result<redis::cluster::ClusterClient> {
    use redis::cluster::ClusterClientBuilder;
    use redis::TlsMode;

    // One seed node; the cluster config endpoint resolves to any
    // alive master and CLUSTER SLOTS discovers the rest on first use.
    let url = if env.tls {
        format!("rediss://{}:{}", env.host, env.port)
    } else {
        format!("redis://{}:{}", env.host, env.port)
    };
    let mut builder = ClusterClientBuilder::new(vec![url]);
    if env.tls {
        builder = builder.tls(TlsMode::Secure);
    }
    builder.build().context("redis ClusterClientBuilder.build()")
}

/// Standard report-writer for cluster runs — wraps
/// `shared::write_report` but stuffs cluster-topology metadata into
/// the report config so downstream consumers can tell a cluster
/// run's JSON from a localhost run's JSON without filename
/// archaeology.
#[allow(clippy::too_many_arguments)]
pub fn write_cluster_report(
    results_dir: &str,
    scenario: &str,
    system: &str,
    mut config: serde_json::Value,
    env: &ClusterEnv,
    duration: std::time::Duration,
    total_ops: u64,
    total_errors: u64,
    lat: &super::shared::HdrSnapshot,
    notes: &str,
) -> Result<std::path::PathBuf> {
    if let serde_json::Value::Object(m) = &mut config {
        m.insert("cluster_mode".into(), serde_json::json!(env.cluster));
        m.insert("tls".into(), serde_json::json!(env.tls));
        m.insert("endpoint".into(), serde_json::json!(env.host));
        m.insert("port".into(), serde_json::json!(env.port));
    }
    super::shared::write_report(
        results_dir,
        scenario,
        system,
        config,
        duration,
        total_ops,
        total_errors,
        lat,
        notes,
    )
}
