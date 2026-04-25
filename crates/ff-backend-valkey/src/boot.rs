//! RFC-017 Wave 8 Stage D — deployment-initialisation (boot) steps.
//!
//! These steps previously lived inline in
//! `ff_server::Server::start_with_metrics`; Stage D relocates them behind
//! [`ValkeyBackend::initialize_deployment`] so that the trait abstraction
//! owns the Valkey-specific boot primitives (RFC-017 §4 row 12).
//!
//! **Ordering contract (load-bearing, RFC-017 §4 row 12):**
//!
//! 1. `verify_valkey_version` — reject pre-7.2 deployments (RFC-011 §13).
//! 2. `validate_or_create_partition_config` — lock partition counts into
//!    `ff:config:partitions` or verify they match.
//! 3. `initialize_waitpoint_hmac_secret` — install the HMAC secret on
//!    every execution partition. **Must precede the lanes seed**: the
//!    pending-waitpoint reads that the unblock scanner triggers on lane
//!    enqueue expect a secret to be installed.
//! 4. `ensure_library` (FUNCTION LOAD) — load the flowfabric Lua library.
//!    **Must precede lanes SADD**: the SADD scripts reference library
//!    functions.
//! 5. Lanes SADD seed — seed `ff:idx:lanes` with the configured lanes.
//!
//! The order matches the pre-relocation `Server::start_with_metrics`
//! body byte-for-byte; this module exists to move the call sites
//! without altering semantics.

use std::collections::HashMap;
use std::time::Duration;

use ferriskey::{Client, Value};
use ff_core::engine_error::{backend_context, EngineError, ValidationKind};
use ff_core::keys::{self, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily};
use ff_core::types::LaneId;
use ff_script::engine_error_ext::transport_script;
use ff_script::error::ScriptError;

/// Minimum Valkey version the engine requires (RFC-011 §13). 7.2 is the
/// release where Valkey Functions + RESP3 stabilised — the primitives
/// the co-location design and typed FCALL wrappers depend on.
pub const REQUIRED_VALKEY_MAJOR: u32 = 7;
pub const REQUIRED_VALKEY_MINOR: u32 = 2;

/// Upper bound on the rolling-upgrade retry window (RFC-011 §9.17).
const VERSION_CHECK_RETRY_BUDGET: Duration = Duration::from_secs(60);

/// Stable initial kid written on first boot; rotation promotes to k2, k3, …
const WAITPOINT_HMAC_INITIAL_KID: &str = "k1";

/// Bounded in-flight concurrency for the startup HMAC-install fan-out.
/// Matches the pre-relocation `BOOT_INIT_CONCURRENCY = 16`.
const BOOT_INIT_CONCURRENCY: usize = 16;

/// Map a `ferriskey::Error` → `EngineError::Transport { backend: "valkey", … }`.
fn transport_fk(e: ferriskey::Error) -> EngineError {
    transport_script(ScriptError::Valkey(e))
}

/// Run the five deployment-init steps in the order mandated by
/// RFC-017 §4 row 12. Fails fast on the first error; ordering is
/// load-bearing — do NOT reorder.
pub async fn initialize_deployment_steps(
    client: &Client,
    partition_config: &PartitionConfig,
    waitpoint_hmac_secret: &str,
    lanes: &[LaneId],
    skip_library_load: bool,
) -> Result<(), EngineError> {
    // Step 1: version gate. Runs before everything else so an
    // unsupported backend fails loud before we start writing config.
    verify_valkey_version(client).await?;

    // Step 2: lock in the deployment's partition counts (or verify).
    validate_or_create_partition_config(client, partition_config).await?;

    // Step 3: HMAC secret install. Precedes lanes seed + library load
    // so pending-waitpoint flows always find a secret on their
    // partition.
    initialize_waitpoint_hmac_secret(client, partition_config, waitpoint_hmac_secret).await?;

    // Step 4: FUNCTION LOAD the flowfabric Lua library. Precedes lanes
    // SADD because lane-referencing scripts resolve function handles.
    if !skip_library_load {
        tracing::info!("loading flowfabric Lua library");
        ff_script::loader::ensure_library(client)
            .await
            .map_err(load_error_to_engine)?;
    } else {
        tracing::info!("skipping library load (skip_library_load=true)");
    }

    // Step 5: seed `ff:idx:lanes`. SADD is idempotent; `Lua` path also
    // SADDs on first-sight for dynamic lanes, this block covers the
    // pre-declared set. `ff:idx:lanes` is intentionally NOT hash-tagged
    // per partition — single cross-partition global SET.
    if !lanes.is_empty() {
        let lane_strs: Vec<&str> = lanes.iter().map(|l| l.as_str()).collect();
        let _: i64 = client
            .cmd("SADD")
            .arg(keys::lanes_index_key().as_str())
            .arg(lane_strs.as_slice())
            .execute()
            .await
            .map_err(|e| backend_context(transport_fk(e), "SADD ff:idx:lanes (seed)"))?;
        tracing::info!(
            lanes = ?lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "seeded lanes index (ff:idx:lanes)"
        );
    }

    Ok(())
}

/// Map `LoadError` → `EngineError`. Shared between the internal
/// `initialize_deployment_steps` step 4 and the trait-surface
/// `EngineBackend::prepare` impl (issue #281).
pub(crate) fn load_error_to_engine(err: ff_script::loader::LoadError) -> EngineError {
    use ff_script::loader::LoadError;
    match err {
        LoadError::Valkey(fk) => backend_context(transport_fk(fk), "FUNCTION LOAD (flowfabric lib)"),
        LoadError::VersionMismatch { expected, got } => EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "flowfabric lua library version mismatch after FUNCTION LOAD: expected {expected}, got {got}"
            ),
        },
    }
}

/// Verify the connected Valkey reports a version ≥ 7.2 (RFC-011 §13).
/// Tolerates rolling upgrades via a 60 s exponential-backoff budget.
pub(crate) async fn verify_valkey_version(client: &Client) -> Result<(), EngineError> {
    let deadline = tokio::time::Instant::now() + VERSION_CHECK_RETRY_BUDGET;
    let mut backoff = Duration::from_millis(200);
    loop {
        let (should_retry, err_for_budget_exhaust, log_detail): (bool, EngineError, String) =
            match query_valkey_version(client).await {
                Ok((detected_major, detected_minor))
                    if (detected_major, detected_minor)
                        >= (REQUIRED_VALKEY_MAJOR, REQUIRED_VALKEY_MINOR) =>
                {
                    tracing::info!(
                        detected_major,
                        detected_minor,
                        required_major = REQUIRED_VALKEY_MAJOR,
                        required_minor = REQUIRED_VALKEY_MINOR,
                        "Valkey version accepted"
                    );
                    return Ok(());
                }
                Ok((detected_major, detected_minor)) => (
                    true,
                    EngineError::Validation {
                        kind: ValidationKind::Corruption,
                        detail: format!(
                            "valkey_version_too_low: detected {detected_major}.{detected_minor}, \
                             required >= {REQUIRED_VALKEY_MAJOR}.{REQUIRED_VALKEY_MINOR} (RFC-011 §13)"
                        ),
                    },
                    format!(
                        "detected={detected_major}.{detected_minor} < required={REQUIRED_VALKEY_MAJOR}.{REQUIRED_VALKEY_MINOR}"
                    ),
                ),
                Err(e) => {
                    // Retry only if the underlying transport is retryable.
                    let retryable = ff_script::engine_error_ext::valkey_kind(&e)
                        .map(ff_script::retry::is_retryable_kind)
                        .unwrap_or(true);
                    let detail = e.to_string();
                    (retryable, e, detail)
                }
            };

        if !should_retry || tokio::time::Instant::now() >= deadline {
            return Err(err_for_budget_exhaust);
        }
        tracing::warn!(
            backoff_ms = backoff.as_millis() as u64,
            detail = %log_detail,
            "valkey version check transient failure; retrying"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn query_valkey_version(client: &Client) -> Result<(u32, u32), EngineError> {
    let raw: Value = client
        .cmd("INFO")
        .arg("server")
        .execute()
        .await
        .map_err(|e| backend_context(transport_fk(e), "INFO server"))?;
    let bodies = extract_info_bodies(&raw)?;
    let mut min_version: Option<(u32, u32)> = None;
    for body in &bodies {
        let version = parse_valkey_version(body)?;
        min_version = Some(match min_version {
            None => version,
            Some(existing) => existing.min(version),
        });
    }
    min_version.ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: "valkey version check: cluster INFO returned no node bodies".into(),
    })
}

/// Normalise an `INFO server` response to one body per node. Cluster
/// (RESP3 map keyed by node address) returns every node's body; we
/// min-reduce so a stale pre-upgrade replica cannot hide behind an
/// upgraded primary.
pub(crate) fn extract_info_bodies(raw: &Value) -> Result<Vec<String>, EngineError> {
    match raw {
        Value::BulkString(bytes) => Ok(vec![String::from_utf8_lossy(bytes).into_owned()]),
        Value::VerbatimString { text, .. } => Ok(vec![text.clone()]),
        Value::SimpleString(s) => Ok(vec![s.clone()]),
        Value::Map(entries) => {
            if entries.is_empty() {
                return Err(EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: "valkey version check: cluster INFO returned empty map".into(),
                });
            }
            let mut out = Vec::with_capacity(entries.len());
            for (_, body) in entries {
                out.extend(extract_info_bodies(body)?);
            }
            Ok(out)
        }
        other => Err(EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("valkey version check: unexpected INFO shape: {other:?}"),
        }),
    }
}

/// Parse `(major, minor)` from an `INFO server` body. Prefers
/// `valkey_version:` (Valkey 8.0+ authoritative); falls back to
/// `redis_version:` only when `server_name:valkey` is present
/// (required from Valkey 7.2 onward). Rejects Redis (no
/// `server_name:valkey` + no `valkey_version`).
pub(crate) fn parse_valkey_version(info: &str) -> Result<(u32, u32), EngineError> {
    let extract_major_minor = |line: &str| -> Result<(u32, u32), EngineError> {
        let trimmed = line.trim();
        let mut parts = trimmed.split('.');
        let major_str = parts.next().unwrap_or("").trim();
        if major_str.is_empty() {
            return Err(EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("valkey version check: empty version field in '{trimmed}'"),
            });
        }
        let major = major_str.parse::<u32>().map_err(|_| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("valkey version check: non-numeric major in '{trimmed}'"),
        })?;
        let minor_str = parts.next().unwrap_or("").trim();
        if minor_str.is_empty() {
            return Err(EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("valkey version check: missing minor component in '{trimmed}'"),
            });
        }
        let minor = minor_str.parse::<u32>().map_err(|_| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("valkey version check: non-numeric minor in '{trimmed}'"),
        })?;
        Ok((major, minor))
    };

    if let Some(valkey_line) = info
        .lines()
        .find_map(|line| line.strip_prefix("valkey_version:"))
    {
        return extract_major_minor(valkey_line);
    }
    let server_is_valkey = info
        .lines()
        .map(str::trim)
        .any(|line| line.eq_ignore_ascii_case("server_name:valkey"));
    if !server_is_valkey {
        return Err(EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: "valkey version check: INFO missing valkey_version and server_name:valkey \
                     marker (unsupported backend — FlowFabric requires Valkey >= 7.2; Redis is not \
                     supported)"
                .into(),
        });
    }
    if let Some(redis_line) = info
        .lines()
        .find_map(|line| line.strip_prefix("redis_version:"))
    {
        return extract_major_minor(redis_line);
    }
    Err(EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: "valkey version check: INFO has server_name:valkey but no redis_version or \
                 valkey_version field"
            .into(),
    })
}

// ── Partition config validation ──────────────────────────────────────

/// Validate-or-create `ff:config:partitions`. On first boot: installs
/// the configured counts. On subsequent boots: rejects any drift
/// between stored and configured counts (partition counts are fixed at
/// deployment time).
async fn validate_or_create_partition_config(
    client: &Client,
    config: &PartitionConfig,
) -> Result<(), EngineError> {
    let key = keys::global_config_partitions();

    let existing: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| backend_context(transport_fk(e), format!("HGETALL {key}")))?;

    if existing.is_empty() {
        tracing::info!("first boot: creating {key}");
        client
            .hset(&key, "num_flow_partitions", &config.num_flow_partitions.to_string())
            .await
            .map_err(|e| backend_context(transport_fk(e), "HSET num_flow_partitions"))?;
        client
            .hset(&key, "num_budget_partitions", &config.num_budget_partitions.to_string())
            .await
            .map_err(|e| backend_context(transport_fk(e), "HSET num_budget_partitions"))?;
        client
            .hset(&key, "num_quota_partitions", &config.num_quota_partitions.to_string())
            .await
            .map_err(|e| backend_context(transport_fk(e), "HSET num_quota_partitions"))?;
        return Ok(());
    }

    let check = |field: &str, expected: u16| -> Result<(), EngineError> {
        let stored: u16 = existing.get(field).and_then(|v| v.parse().ok()).unwrap_or(0);
        if stored != expected {
            return Err(EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!(
                    "partition_mismatch: {field}: stored={stored}, config={expected}. \
                     Partition counts are fixed at deployment time. \
                     Either fix your config or migrate the data."
                ),
            });
        }
        Ok(())
    };

    check("num_flow_partitions", config.num_flow_partitions)?;
    check("num_budget_partitions", config.num_budget_partitions)?;
    check("num_quota_partitions", config.num_quota_partitions)?;

    tracing::info!("partition config validated against stored {key}");
    Ok(())
}

// ── Waitpoint HMAC secret bootstrap (RFC-004 §Waitpoint Security) ────

enum PartitionBootOutcome {
    Match,
    Mismatch,
    Repaired,
    Installed,
}

async fn init_one_partition(
    client: &Client,
    partition: Partition,
    secret_hex: &str,
) -> Result<PartitionBootOutcome, EngineError> {
    let key = IndexKeys::new(&partition).waitpoint_hmac_secrets();

    let stored_kid: Option<String> = client
        .cmd("HGET")
        .arg(&key)
        .arg("current_kid")
        .execute()
        .await
        .map_err(|e| backend_context(transport_fk(e), format!("HGET {key} current_kid (init probe)")))?;

    if let Some(stored_kid) = stored_kid {
        let field = format!("secret:{stored_kid}");
        let stored_secret: Option<String> = client
            .hget(&key, &field)
            .await
            .map_err(|e| backend_context(transport_fk(e), format!("HGET {key} secret:<kid> (init check)")))?;
        if stored_secret.is_none() {
            // Torn write from a prior boot: repair in place.
            client
                .hset(&key, &field, secret_hex)
                .await
                .map_err(|e| backend_context(transport_fk(e), format!("HSET {key} secret:<kid> (repair torn write)")))?;
            return Ok(PartitionBootOutcome::Repaired);
        }
        if stored_secret.as_deref() != Some(secret_hex) {
            return Ok(PartitionBootOutcome::Mismatch);
        }
        return Ok(PartitionBootOutcome::Match);
    }

    // Fresh partition — install current_kid + secret:<kid> atomically.
    let secret_field = format!("secret:{WAITPOINT_HMAC_INITIAL_KID}");
    let _: i64 = client
        .cmd("HSET")
        .arg(&key)
        .arg("current_kid")
        .arg(WAITPOINT_HMAC_INITIAL_KID)
        .arg(&secret_field)
        .arg(secret_hex)
        .execute()
        .await
        .map_err(|e| backend_context(transport_fk(e), format!("HSET {key} (init waitpoint HMAC atomic)")))?;
    Ok(PartitionBootOutcome::Installed)
}

async fn initialize_waitpoint_hmac_secret(
    client: &Client,
    partition_config: &PartitionConfig,
    secret_hex: &str,
) -> Result<(), EngineError> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let n = partition_config.num_flow_partitions;
    tracing::info!(
        partitions = n,
        concurrency = BOOT_INIT_CONCURRENCY,
        "installing waitpoint HMAC secret across {n} execution partitions"
    );

    let mut mismatch_count: u16 = 0;
    let mut repaired_count: u16 = 0;
    let mut pending: FuturesUnordered<_> = FuturesUnordered::new();
    let mut next_index: u16 = 0;

    loop {
        while pending.len() < BOOT_INIT_CONCURRENCY && next_index < n {
            let partition = Partition {
                family: PartitionFamily::Execution,
                index: next_index,
            };
            let client = client.clone();
            let secret_hex = secret_hex.to_owned();
            pending.push(async move { init_one_partition(&client, partition, &secret_hex).await });
            next_index += 1;
        }
        match pending.next().await {
            Some(res) => match res? {
                PartitionBootOutcome::Match | PartitionBootOutcome::Installed => {}
                PartitionBootOutcome::Mismatch => mismatch_count += 1,
                PartitionBootOutcome::Repaired => repaired_count += 1,
            },
            None => break,
        }
    }

    if repaired_count > 0 {
        tracing::warn!(
            repaired_partitions = repaired_count,
            total_partitions = n,
            "repaired {repaired_count} partitions with torn waitpoint HMAC writes \
             (current_kid present but secret:<kid> missing, likely crash during prior boot)"
        );
    }
    if mismatch_count > 0 {
        tracing::warn!(
            mismatched_partitions = mismatch_count,
            total_partitions = n,
            "stored/env secret mismatch on {mismatch_count} partitions — \
             env FF_WAITPOINT_HMAC_SECRET ignored in favor of stored values; \
             run POST /v1/admin/rotate-waitpoint-secret to sync"
        );
    }

    tracing::info!(partitions = n, "waitpoint HMAC secret install complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::value::VerbatimFormat;

    #[test]
    fn parse_valkey_version_prefers_valkey_version_over_redis_version() {
        let info = "# Server\r\nredis_version:7.2.4\r\nvalkey_version:9.0.3\r\nserver_mode:cluster\r\nos:Linux\r\n";
        assert_eq!(parse_valkey_version(info).unwrap(), (9, 0));
    }

    #[test]
    fn parse_valkey_version_real_valkey_8_cluster_body() {
        let info = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:9.0.3\r\nvalkey_release_stage:ga\r\nredis_git_sha1:00000000\r\nserver_mode:cluster\r\n";
        assert_eq!(parse_valkey_version(info).unwrap(), (9, 0));
    }

    #[test]
    fn parse_valkey_version_falls_back_to_redis_version_on_valkey_7() {
        let info = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nfoo:bar\r\n";
        assert_eq!(parse_valkey_version(info).unwrap(), (7, 2));
    }

    #[test]
    fn parse_valkey_version_rejects_redis_backend() {
        let info = "# Server\r\nredis_version:7.4.0\r\nredis_mode:standalone\r\nos:Linux\r\n";
        let err = parse_valkey_version(info).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Redis is not supported") && msg.contains("server_name:valkey"),
            "expected Redis-rejection message, got: {msg}"
        );
    }

    #[test]
    fn parse_valkey_version_accepts_valkey_7_marker_case_insensitively() {
        let info = "redis_version:7.2.0\r\nSERVER_NAME:Valkey\r\n";
        assert_eq!(parse_valkey_version(info).unwrap(), (7, 2));
    }

    #[test]
    fn parse_valkey_version_errors_when_no_version_field() {
        let info = "# Server\r\nfoo:bar\r\n";
        let err = parse_valkey_version(info).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn parse_valkey_version_errors_on_non_numeric_major() {
        let info = "valkey_version:invalid.x.y\n";
        let err = parse_valkey_version(info).unwrap_err();
        assert!(err.to_string().contains("non-numeric major"));
    }

    #[test]
    fn parse_valkey_version_errors_on_non_numeric_minor() {
        let info = "valkey_version:7.x.0\n";
        let err = parse_valkey_version(info).unwrap_err();
        assert!(err.to_string().contains("non-numeric minor"));
    }

    #[test]
    fn parse_valkey_version_errors_on_missing_minor() {
        let info = "valkey_version:7\n";
        let err = parse_valkey_version(info).unwrap_err();
        assert!(err.to_string().contains("missing minor"));
    }

    #[test]
    fn extract_info_bodies_unwraps_cluster_map_all_entries() {
        let body_a = "# Server\r\nredis_version:7.2.4\r\nvalkey_version:9.0.3\r\n";
        let body_b = "# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.0\r\n";
        let map = Value::Map(vec![
            (
                Value::SimpleString("127.0.0.1:7000".to_string()),
                Value::VerbatimString { format: VerbatimFormat::Text, text: body_a.to_string() },
            ),
            (
                Value::SimpleString("127.0.0.1:7001".to_string()),
                Value::VerbatimString { format: VerbatimFormat::Text, text: body_b.to_string() },
            ),
        ]);
        let bodies = extract_info_bodies(&map).unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], body_a);
        assert_eq!(bodies[1], body_b);
    }

    #[test]
    fn extract_info_bodies_handles_simple_string() {
        let body_text = "redis_version:7.2.4\r\nvalkey_version:9.0.3\r\n";
        let v = Value::SimpleString(body_text.to_string());
        assert_eq!(extract_info_bodies(&v).unwrap(), vec![body_text.to_string()]);
    }

    #[test]
    fn extract_info_bodies_rejects_empty_cluster_map() {
        let map = Value::Map(vec![]);
        let err = extract_info_bodies(&map).unwrap_err();
        assert!(err.to_string().contains("empty map"));
    }

    #[test]
    fn parse_valkey_version_min_across_cluster_map_picks_lowest() {
        let body_node1 = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.0.0\r\n";
        let body_node2 = "# Server\r\nredis_version:7.1.0\r\nserver_name:valkey\r\n";
        let body_node3 = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:7.2.0\r\n";
        let map = Value::Map(vec![
            (Value::SimpleString("node1:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node1.into() }),
            (Value::SimpleString("node2:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node2.into() }),
            (Value::SimpleString("node3:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node3.into() }),
        ]);
        let bodies = extract_info_bodies(&map).unwrap();
        let min = bodies.iter().map(|b| parse_valkey_version(b).unwrap()).min().unwrap();
        assert_eq!(min, (7, 1));
        assert!(min < (REQUIRED_VALKEY_MAJOR, REQUIRED_VALKEY_MINOR));
    }

    #[test]
    fn parse_valkey_version_all_nodes_at_or_above_floor_accepts() {
        let body_node1 = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.0.0\r\n";
        let body_node2 = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:7.2.0\r\n";
        let body_node3 = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:9.0.3\r\n";
        let map = Value::Map(vec![
            (Value::SimpleString("node1:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node1.into() }),
            (Value::SimpleString("node2:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node2.into() }),
            (Value::SimpleString("node3:6379".into()), Value::VerbatimString { format: VerbatimFormat::Text, text: body_node3.into() }),
        ]);
        let bodies = extract_info_bodies(&map).unwrap();
        let min = bodies.iter().map(|b| parse_valkey_version(b).unwrap()).min().unwrap();
        assert_eq!(min, (7, 2));
        assert!(min >= (REQUIRED_VALKEY_MAJOR, REQUIRED_VALKEY_MINOR));
    }
}
