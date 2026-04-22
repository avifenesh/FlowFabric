//! Thin wrapper around `ff_test::fixtures::TestCluster`.
//!
//! PR-D1 keeps this a passthrough — future PRs (D2+) may add
//! version-probe + matrix-cell plumbing here.

pub use ff_test::fixtures::{
    backend_config_from_env, build_client_from_env, env_flag, TestCluster, TEST_PARTITION_CONFIG,
};

/// Probe Valkey's server version. Prefers `valkey_version:` from INFO,
/// falls back to `redis_version:` for older or Redis-OSS deployments.
/// Mirrors the parser in `ff-server::server`.
pub async fn probe_server_version(client: &ferriskey::Client) -> Option<String> {
    let info: ferriskey::Value = client
        .cmd("INFO")
        .arg("server")
        .execute()
        .await
        .ok()?;
    let text = match info {
        ferriskey::Value::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
        ferriskey::Value::SimpleString(s) => s,
        ferriskey::Value::VerbatimString { text, .. } => text,
        _ => return None,
    };
    // Prefer valkey_version; fall back to redis_version.
    for prefix in ["valkey_version:", "redis_version:"] {
        if let Some(line) = text.lines().find(|l| l.starts_with(prefix)) {
            return Some(line.trim_start_matches(prefix).trim().to_owned());
        }
    }
    None
}
