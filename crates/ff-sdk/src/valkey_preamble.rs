//! Valkey-specific connect preamble for [`FlowFabricWorker::connect`].
//!
//! v0.12 backend-agnostic SDK PR-6: the Valkey-specific observable work
//! that `connect` used to do inline — PING, SET-NX alive-key guard,
//! `ff:config:partitions` HGETALL, capability ingress validation + CSV
//! compute, and the `ff:worker:{id}:caps` advertisement / `ff:idx:workers`
//! index write — lives here as a single [`run`] entry point so
//! `connect`'s body shrinks to three statements (build_client, run
//! preamble, wrap backend).
//!
//! The module is `#[cfg(feature = "valkey-default")]`-gated: a
//! `--no-default-features --features sqlite` build does not compile it,
//! matching the gate on [`FlowFabricWorker::connect`] itself.
//!
//! **Behaviour preservation.** The write order is observable from the
//! scheduler side (`unblock` scanner reads `ff:idx:workers` then GETs
//! `ff:worker:{id}:caps`). The extraction is byte-for-byte identical to
//! the pre-PR-6 inline body — see the PR-6 description in the archive
//! repo for the key-write diff.
//!
//! [`FlowFabricWorker::connect`]: crate::FlowFabricWorker::connect

use std::collections::HashMap;

use ferriskey::Client;
use ff_core::partition::PartitionConfig;

use crate::config::WorkerConfig;
use crate::SdkError;

/// Output of [`run`] — the bits `connect` threads back into the
/// [`FlowFabricWorker`] struct.
///
/// v0.14: `capabilities_csv` / `capabilities_hash` are always-on —
/// `claim_next_via_backend` needs the hash for its per-mismatch log
/// surface regardless of which claim path the worker uses.
///
/// The `ff:worker:{id}:caps` + `ff:idx:workers` advertisement writes
/// also run on every worker (v0.13 ungate) so the unblock scanner has
/// caps keys regardless of direct-claim vs server-routed claim.
///
/// [`FlowFabricWorker`]: crate::FlowFabricWorker
pub(crate) struct PreambleOutput {
    pub partition_config: PartitionConfig,
    pub capabilities_csv: String,
    pub capabilities_hash: String,
}

/// Run the Valkey-specific `connect` preamble against an already-dialed
/// `ferriskey::Client`.
///
/// Executes (in order — the order is observable from scheduler reads):
/// 1. `PING` connectivity probe.
/// 2. `SET NX PX` on `ff:worker:{instance_id}:alive` (duplicate-instance
///    guard, TTL = 2 × `lease_ttl_ms`).
/// 3. `HGETALL ff:config:partitions` (falls back to `PartitionConfig::default()`
///    on absence, which is the SDK-only-test shape).
/// 4. Capability ingress validation + sorted-dedup CSV compute
///    (always-on; v0.13 ungate). Under `direct-valkey-claim` the FNV-1a
///    digest + full-CSV connect-time log also run.
/// 5. `SET`/`DEL` of `ff:worker:{instance_id}:caps` + `SADD`/`SREM` of
///    the `ff:idx:workers` index (always-on; v0.13 ungate — needed by
///    the unblock scanner's `load_worker_caps_union` on both
///    direct-claim and server-routed worker paths). Caps-first write
///    order is load-bearing.
pub(crate) async fn run(
    client: &Client,
    config: &WorkerConfig,
) -> Result<PreambleOutput, SdkError> {
    // Verify connectivity
    let pong: String = client
        .cmd("PING")
        .execute()
        .await
        .map_err(|e| crate::backend_context(e, "PING failed"))?;
    if pong != "PONG" {
        return Err(SdkError::Config {
            context: "worker_connect".into(),
            field: None,
            message: format!("unexpected PING response: {pong}"),
        });
    }

    // Guard against two worker processes sharing the same
    // `worker_instance_id`. See `FlowFabricWorker::connect` rustdoc for
    // the full three limitations (startup-only, restart delay, no
    // graceful cleanup) — documented at the caller.
    let alive_key = format!("ff:worker:{}:alive", config.worker_instance_id);
    let alive_ttl_ms = (config.lease_ttl_ms.saturating_mul(2)).max(1_000);
    let set_result: Option<String> = client
        .cmd("SET")
        .arg(&alive_key)
        .arg("1")
        .arg("NX")
        .arg("PX")
        .arg(alive_ttl_ms.to_string().as_str())
        .execute()
        .await
        .map_err(|e| crate::backend_context(e, "SET NX worker alive key"))?;
    if set_result.is_none() {
        return Err(SdkError::Config {
            context: "worker_connect".into(),
            field: Some("worker_instance_id".into()),
            message: format!(
                "duplicate worker_instance_id '{}': another process already holds {alive_key}",
                config.worker_instance_id
            ),
        });
    }

    // Read partition config from Valkey (set by ff-server on startup).
    // Falls back to defaults if key doesn't exist (e.g. SDK-only testing).
    let partition_config = read_partition_config(client).await.unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "ff:config:partitions not found, using defaults"
        );
        PartitionConfig::default()
    });

    // Capability ingress validation + CSV compute — mirrors
    // `Scheduler::claim_for_worker` (ff-scheduler). Reject empty
    // tokens, `,`-bearing tokens, and whitespace/control chars so
    // operator misconfig fails loud at boot instead of silently
    // mis-routing forever.
    //
    // v0.13 ungate: these run on every worker, not just the
    // `direct-valkey-claim` feature, because the server-routed claim
    // path needs `ff:idx:workers` + `ff:worker:{id}:caps` populated for
    // the unblock scanner's `load_worker_caps_union` read to promote
    // `blocked_by_route` executions with `required_capabilities` →
    // `runnable`. Without the writes, first-stage execs sit in
    // `blocked_by_route` forever (caught in v0.13 release gate via the
    // `media-pipeline` example).
    for cap in &config.capabilities {
        if cap.is_empty() {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: Some("capabilities".into()),
                message: "capability token must not be empty".into(),
            });
        }
        if cap.contains(',') {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: Some("capabilities".into()),
                message: format!(
                    "capability token may not contain ',' (CSV delimiter): {cap:?}"
                ),
            });
        }
        if cap.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: Some("capabilities".into()),
                message: format!(
                    "capability token must not contain whitespace or control \
                     characters: {cap:?}"
                ),
            });
        }
    }
    let capabilities_csv: String = {
        let set: std::collections::BTreeSet<&str> = config
            .capabilities
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if set.len() > ff_core::policy::CAPS_MAX_TOKENS {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: Some("capabilities".into()),
                message: format!(
                    "capability set exceeds CAPS_MAX_TOKENS ({}): {}",
                    ff_core::policy::CAPS_MAX_TOKENS,
                    set.len()
                ),
            });
        }
        let csv = set.into_iter().collect::<Vec<_>>().join(",");
        if csv.len() > ff_core::policy::CAPS_MAX_BYTES {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: Some("capabilities".into()),
                message: format!(
                    "capability CSV exceeds CAPS_MAX_BYTES ({}): {}",
                    ff_core::policy::CAPS_MAX_BYTES,
                    csv.len()
                ),
            });
        }
        csv
    };

    let capabilities_hash = ff_core::hash::fnv1a_xor8hex(&capabilities_csv);

    if !capabilities_csv.is_empty() {
        tracing::info!(
            worker_instance_id = %config.worker_instance_id,
            worker_caps_hash = %capabilities_hash,
            worker_caps = %capabilities_csv,
            "worker connected with capabilities (full CSV — mismatch logs use hash only)"
        );
    }

    // Advertisement of caps for the unblock scanner + operator
    // visibility (CLI introspection, dashboards). Per Phase 3d of
    // cairn #453 the scanner's `load_worker_caps_union` drives
    // `blocked_by_route → runnable` promotion; server-routed workers
    // need these keys populated too, not just `direct-valkey-claim`
    // ones. The AUTHORITATIVE source for scheduling decisions at
    // claim-time is still ARGV[9] on each claim. The caps STRING is
    // written BEFORE the index SADD so that when the scheduler's
    // unblock scanner observes the id in the index, the caps key is
    // guaranteed to resolve to a non-stale CSV.
    {
        let caps_key = ff_core::keys::worker_caps_key(&config.worker_instance_id);
        let index_key = ff_core::keys::workers_index_key();
        let instance_id = config.worker_instance_id.to_string();
        if capabilities_csv.is_empty() {
            let _ = client
                .cmd("DEL")
                .arg(&caps_key)
                .execute::<Option<i64>>()
                .await;
            if let Err(e) = client
                .cmd("SREM")
                .arg(&index_key)
                .arg(&instance_id)
                .execute::<Option<i64>>()
                .await
            {
                tracing::warn!(error = %e, key = %index_key, instance = %instance_id,
                    "SREM workers-index failed; continuing (non-authoritative)");
            }
        } else {
            if let Err(e) = client
                .cmd("SET")
                .arg(&caps_key)
                .arg(&capabilities_csv)
                .execute::<Option<String>>()
                .await
            {
                tracing::warn!(error = %e, key = %caps_key,
                    "SET worker caps advertisement failed; continuing");
            }
            if let Err(e) = client
                .cmd("SADD")
                .arg(&index_key)
                .arg(&instance_id)
                .execute::<Option<i64>>()
                .await
            {
                tracing::warn!(error = %e, key = %index_key, instance = %instance_id,
                    "SADD workers-index failed; continuing");
            }
        }
    }

    Ok(PreambleOutput {
        partition_config,
        capabilities_csv,
        capabilities_hash,
    })
}

/// Read partition config from Valkey's `ff:config:partitions` hash.
/// Returns `Err` if the key doesn't exist or can't be read — `run`
/// catches this and falls back to `PartitionConfig::default()`.
async fn read_partition_config(client: &Client) -> Result<PartitionConfig, SdkError> {
    let key = ff_core::keys::global_config_partitions();
    let fields: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| crate::backend_context(e, format!("HGETALL {key}")))?;

    if fields.is_empty() {
        return Err(SdkError::Config {
            context: "read_partition_config".into(),
            field: None,
            message: "ff:config:partitions not found in Valkey".into(),
        });
    }

    let parse = |field: &str, default: u16| -> u16 {
        fields
            .get(field)
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u16| n > 0)
            .unwrap_or(default)
    };

    Ok(PartitionConfig {
        num_flow_partitions: parse("num_flow_partitions", 256),
        num_budget_partitions: parse("num_budget_partitions", 32),
        num_quota_partitions: parse("num_quota_partitions", 32),
    })
}
