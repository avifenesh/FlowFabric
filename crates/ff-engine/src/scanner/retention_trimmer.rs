//! Terminal execution retention scanner.
//!
//! Scans `ff:idx:{p:N}:lane:<lane>:terminal` for each partition+lane,
//! finding terminal executions whose `completed_at` score is older than
//! the configured retention period. For each, cascading-deletes all
//! sub-objects: streams, attempt hashes, usage, policy, lease, suspension,
//! waitpoints, signals, and finally the exec core + index entries.
//!
//! This is a Rust-only scanner — no FCALL needed. Uses direct Valkey
//! commands (ZRANGEBYSCORE + HGET + DEL).
//!
//! Reference: RFC-010 §6.12

use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::LaneId;

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 20;

/// Default retention period: 24 hours.
const DEFAULT_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

pub struct RetentionTrimmer {
    interval: Duration,
    lanes: Vec<LaneId>,
    /// Default retention period in ms (used when execution has no policy).
    default_retention_ms: u64,
}

impl RetentionTrimmer {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self {
            interval,
            lanes,
            default_retention_ms: DEFAULT_RETENTION_MS,
        }
    }
}

impl Scanner for RetentionTrimmer {
    fn name(&self) -> &'static str {
        "retention_trimmer"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn scan_partition(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> ScanResult {
        let p = Partition {
            family: PartitionFamily::Execution,
            index: partition,
        };
        let idx = IndexKeys::new(&p);

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "retention_trimmer: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        for lane in &self.lanes {
            let terminal_key = idx.lane_terminal(lane);

            // Find executions that completed before the retention cutoff.
            // Score = completed_at ms. We look for score <= (now - default_retention).
            // Per-execution retention override is checked after fetching.
            let cutoff = now_ms.saturating_sub(self.default_retention_ms);

            let expired: Vec<String> = match client
                .cmd("ZRANGEBYSCORE")
                .arg(&terminal_key)
                .arg("-inf")
                .arg(cutoff.to_string().as_str())
                .arg("LIMIT")
                .arg("0")
                .arg(BATCH_SIZE.to_string().as_str())
                .execute()
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(
                        partition, lane = lane.as_str(), error = %e,
                        "retention_trimmer: ZRANGEBYSCORE failed"
                    );
                    total_errors += 1;
                    continue;
                }
            };

            if expired.is_empty() {
                continue;
            }

            for eid_str in &expired {
                match purge_execution(
                    client, &p, &idx, lane, eid_str, &terminal_key, now_ms,
                    self.default_retention_ms,
                ).await {
                    Ok(true) => total_processed += 1,
                    Ok(false) => {} // skipped (custom retention not yet expired)
                    Err(e) => {
                        tracing::warn!(
                            partition,
                            execution_id = eid_str.as_str(),
                            error = %e,
                            "retention_trimmer: purge failed"
                        );
                        total_errors += 1;
                    }
                }
            }
        }

        ScanResult { processed: total_processed, errors: total_errors }
    }
}

/// Purge all keys for one terminal execution. Returns Ok(true) if purged,
/// Ok(false) if skipped (custom retention not yet due).
#[allow(clippy::too_many_arguments)]
async fn purge_execution(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    _lane: &LaneId,
    eid_str: &str,
    terminal_key: &str,
    now_ms: u64,
    default_retention_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let tag = partition.hash_tag();
    let exec_core_key = format!("ff:exec:{}:{}:core", tag, eid_str);

    // Read completed_at and total_attempt_count from exec_core
    let fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&exec_core_key)
        .arg("completed_at")
        .arg("total_attempt_count")
        .execute()
        .await?;

    let completed_at: u64 = fields.first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let total_attempts: u32 = fields.get(1)
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if completed_at == 0 {
        // No completed_at — exec_core may already be gone. Clean index entry.
        let _: u32 = client.cmd("ZREM").arg(terminal_key).arg(eid_str).execute().await?;
        return Ok(true);
    }

    // Check per-execution retention override from policy key
    let policy_key = format!("ff:exec:{}:{}:policy", tag, eid_str);
    let retention_ms = read_retention_ms(client, &policy_key, default_retention_ms).await;

    if now_ms < completed_at + retention_ms {
        return Ok(false); // Not yet expired under custom retention
    }

    // ── Cascading delete ──
    // Collect subordinate keys first. exec_core and the policy key are
    // held out of this list and DELeted LAST, after every other chunk
    // succeeds. Rationale: if an intermediate chunk fails with a
    // transient error, the next retention pass needs to re-read
    // `exec_core.total_attempt_count` and `policy.retention_ttl_ms` to
    // rebuild the full del_keys list — so we must not destroy those two
    // keys until the rest of the cascade has committed. ZREM on the
    // terminal_zset entry also happens last (existing invariant).
    let mut del_keys: Vec<String> = Vec::with_capacity(16 + total_attempts as usize * 5);

    // Execution-level keys (safe to delete before exec_core)
    del_keys.push(format!("ff:exec:{}:{}:payload", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:result", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:tags", tag, eid_str));

    // Lease keys
    del_keys.push(format!("ff:exec:{}:{}:lease:current", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:lease:history", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:claim_grant", tag, eid_str));

    // Attempt-level keys (for each attempt 0..total_attempts)
    del_keys.push(format!("ff:exec:{}:{}:attempts", tag, eid_str));
    for i in 0..total_attempts {
        del_keys.push(format!("ff:attempt:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:usage", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:policy", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}:meta", tag, eid_str, i));
    }

    // Suspension/waitpoint keys
    del_keys.push(format!("ff:exec:{}:{}:suspension:current", tag, eid_str));

    // Dependency keys (RFC-007)
    // deps:all_edges holds every edge ID ever applied to this exec (never
    // pruned on resolve), so SMEMBERS gives us the full set of dep hashes to
    // drop — cluster-safe, no SCAN.
    let deps_all_edges_key = format!("ff:exec:{}:{}:deps:all_edges", tag, eid_str);
    let dep_edge_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&deps_all_edges_key)
        .execute()
        .await
        .unwrap_or_default();

    del_keys.push(format!("ff:exec:{}:{}:deps:meta", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:deps:unresolved", tag, eid_str));
    del_keys.push(deps_all_edges_key);
    for edge_id in &dep_edge_ids {
        del_keys.push(format!("ff:exec:{}:{}:dep:{}", tag, eid_str, edge_id));
    }

    // Read waitpoints set to discover all waitpoint-related keys
    let waitpoints_key = format!("ff:exec:{}:{}:waitpoints", tag, eid_str);
    let wp_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&waitpoints_key)
        .execute()
        .await
        .unwrap_or_default();

    del_keys.push(waitpoints_key);

    for wp_id_str in &wp_ids {
        del_keys.push(format!("ff:wp:{}:{}", tag, wp_id_str));
        del_keys.push(format!("ff:wp:{}:{}:signals", tag, wp_id_str));
        del_keys.push(format!("ff:wp:{}:{}:condition", tag, wp_id_str));
    }

    // Signal keys — read from per-execution signal index
    let signal_key = format!("ff:exec:{}:{}:signals", tag, eid_str);
    let sig_ids: Vec<String> = client
        .cmd("ZRANGE")
        .arg(&signal_key)
        .arg("0")
        .arg("-1")
        .execute()
        .await
        .unwrap_or_default();

    del_keys.push(signal_key);

    for sig_id_str in &sig_ids {
        del_keys.push(format!("ff:signal:{}:{}", tag, sig_id_str));
        del_keys.push(format!("ff:signal:{}:{}:payload", tag, sig_id_str));
    }

    // Batch DEL in chunks of 500 to avoid oversized commands on
    // executions with many attempts/signals/waitpoints/deps. Subordinate
    // keys go first; if any chunk errors with `?`, exec_core and
    // policy_key remain untouched so the next retention pass can
    // re-read them and rebuild the full del_keys list.
    for chunk in del_keys.chunks(500) {
        let key_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let _: u32 = client
            .cmd("DEL")
            .arg(&key_refs)
            .execute()
            .await?;
    }

    // Finalize: drop exec_core and policy together (both on {p:N}), then
    // sweep the terminal-zset entry and the partition-wide all_executions
    // index. Doing this after the subordinate chunks guarantees that a
    // partial retention failure is idempotently retriable without
    // orphaning keys that retention discovers by reading exec_core.
    let _: u32 = client
        .cmd("DEL")
        .arg(&[exec_core_key.as_str(), policy_key.as_str()][..])
        .execute()
        .await?;

    let _: u32 = client.cmd("ZREM").arg(terminal_key).arg(eid_str).execute().await?;
    let all_exec_key = idx.all_executions();
    let _: u32 = client.cmd("SREM").arg(&all_exec_key).arg(eid_str).execute().await?;

    tracing::debug!(
        execution_id = eid_str,
        attempts = total_attempts,
        waitpoints = wp_ids.len(),
        signals = sig_ids.len(),
        "retention_trimmer: purged execution"
    );

    Ok(true)
}

/// Read retention_ttl_ms from the execution's policy JSON.
/// Returns default_retention_ms if policy doesn't exist or doesn't specify retention.
async fn read_retention_ms(
    client: &ferriskey::Client,
    policy_key: &str,
    default_retention_ms: u64,
) -> u64 {
    let policy_json: Option<String> = match client
        .cmd("GET")
        .arg(policy_key)
        .execute()
        .await
    {
        Ok(v) => v,
        Err(_) => return default_retention_ms,
    };

    let json_str = match policy_json {
        Some(s) if !s.is_empty() => s,
        _ => return default_retention_ms,
    };

    // Parse JSON to extract stream_policy.retention_ttl_ms
    let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return default_retention_ms,
    };

    parsed
        .get("stream_policy")
        .and_then(|sp| sp.get("retention_ttl_ms"))
        .and_then(|v| v.as_u64())
        .unwrap_or(default_retention_ms)
}

