//! Terminal execution retention scanner.
//!
//! Scans `ff:idx:{p:N}:lane:<lane>:terminal` for each partition+lane,
//! finding terminal executions older than the configured retention
//! period. For each batch the per-execution cascade-delete runs
//! through [`EngineBackend::trim_retention`] (Valkey lifts the pre-
//! PR-7b direct-client path; Postgres cascades DELETEs across every
//! sibling table inside one transaction; SQLite is `Unavailable` per
//! RFC-023 Phase 3.5).
//!
//! Retention trimming is inherently a scan-and-delete loop over time.
//! The trait exists to remove engine-side Valkey coupling, NOT to
//! atomise the operation into a single round-trip — implementations
//! may still issue multiple round-trips per batch.
//!
//! Reference: RFC-010 §6.12

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{LaneId, TimestampMs};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 20;

/// Default retention period: 24 hours.
const DEFAULT_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

pub struct RetentionTrimmer {
    interval: Duration,
    lanes: Vec<LaneId>,
    /// Default retention period in ms (used when execution has no policy).
    default_retention_ms: u64,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl RetentionTrimmer {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self::with_filter(interval, lanes, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(interval: Duration, lanes: Vec<LaneId>, filter: ScannerFilter) -> Self {
        Self {
            interval,
            lanes,
            default_retention_ms: DEFAULT_RETENTION_MS,
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 2b-B: wire an `EngineBackend` so the per-batch
    /// retention cascade runs through the trait.
    pub fn with_filter_and_backend(
        interval: Duration,
        lanes: Vec<LaneId>,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            lanes,
            default_retention_ms: DEFAULT_RETENTION_MS,
            filter,
            backend: Some(backend),
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

    fn filter(&self) -> &ScannerFilter {
        &self.filter
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

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "retention_trimmer: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };
        let now_ts = TimestampMs::from_millis(now_ms as i64);

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        for lane in &self.lanes {
            let res = if let Some(backend) = self.backend.as_ref() {
                backend
                    .trim_retention(p, lane, self.default_retention_ms, now_ts, BATCH_SIZE)
                    .await
                    .map_err(|e: EngineError| e.to_string())
            } else {
                // Test-only fallback when the scanner is instantiated
                // without a backend. Mirrors the pre-PR-7b ZRANGEBYSCORE
                // + per-exec cascade loop behaviour for test fixtures.
                trim_retention_direct_fallback(
                    client,
                    &p,
                    lane,
                    self.default_retention_ms,
                    now_ms,
                    BATCH_SIZE,
                )
                .await
                .map_err(|e| e.to_string())
            };

            match res {
                Ok(n) => total_processed += n,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        lane = lane.as_str(),
                        error = %e,
                        "retention_trimmer: trim_retention failed"
                    );
                    total_errors += 1;
                }
            }
        }

        ScanResult { processed: total_processed, errors: total_errors }
    }
}

/// Test-only direct-client retention loop for scanner instantiations
/// without an `EngineBackend`. Returns the number of executions purged
/// in this call.
async fn trim_retention_direct_fallback(
    client: &ferriskey::Client,
    partition: &Partition,
    lane: &LaneId,
    default_retention_ms: u64,
    now_ms: u64,
    batch_size: u32,
) -> Result<u32, ferriskey::Error> {
    let idx = IndexKeys::new(partition);
    let terminal_key = idx.lane_terminal(lane);
    let cutoff = now_ms.saturating_sub(default_retention_ms);

    let expired: Vec<String> = client
        .cmd("ZRANGEBYSCORE")
        .arg(&terminal_key)
        .arg("-inf")
        .arg(cutoff.to_string().as_str())
        .arg("LIMIT")
        .arg("0")
        .arg(batch_size.to_string().as_str())
        .execute()
        .await?;

    if expired.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u32;
    for eid_str in &expired {
        if purge_execution_fallback(
            client,
            partition,
            &idx,
            eid_str,
            &terminal_key,
            now_ms,
            default_retention_ms,
        )
        .await?
        {
            processed += 1;
        }
    }
    Ok(processed)
}

#[allow(clippy::too_many_arguments)]
async fn purge_execution_fallback(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    eid_str: &str,
    terminal_key: &str,
    now_ms: u64,
    default_retention_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let tag = partition.hash_tag();
    let exec_core_key = format!("ff:exec:{}:{}:core", tag, eid_str);

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
        let _: u32 = client.cmd("ZREM").arg(terminal_key).arg(eid_str).execute().await?;
        return Ok(true);
    }

    let policy_key = format!("ff:exec:{}:{}:policy", tag, eid_str);
    let retention_ms = read_retention_ms_fallback(client, &policy_key, default_retention_ms).await;
    if now_ms < completed_at + retention_ms {
        return Ok(false);
    }

    let mut del_keys: Vec<String> = Vec::with_capacity(16 + total_attempts as usize * 5);
    del_keys.push(format!("ff:exec:{}:{}:payload", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:result", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:tags", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:lease:current", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:lease:history", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:claim_grant", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:attempts", tag, eid_str));
    for i in 0..total_attempts {
        del_keys.push(format!("ff:attempt:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:usage", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:policy", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}:meta", tag, eid_str, i));
    }
    del_keys.push(format!("ff:exec:{}:{}:suspension:current", tag, eid_str));

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

    for chunk in del_keys.chunks(500) {
        let key_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let _: u32 = client.cmd("DEL").arg(&key_refs).execute().await?;
    }

    let _: u32 = client
        .cmd("DEL")
        .arg(&[exec_core_key.as_str(), policy_key.as_str()][..])
        .execute()
        .await?;

    let _: u32 = client.cmd("ZREM").arg(terminal_key).arg(eid_str).execute().await?;
    let all_exec_key = idx.all_executions();
    let _: u32 = client.cmd("SREM").arg(&all_exec_key).arg(eid_str).execute().await?;

    Ok(true)
}

async fn read_retention_ms_fallback(
    client: &ferriskey::Client,
    policy_key: &str,
    default_retention_ms: u64,
) -> u64 {
    let policy_json: Option<String> = match client.cmd("GET").arg(policy_key).execute().await {
        Ok(v) => v,
        Err(_) => return default_retention_ms,
    };
    let json_str = match policy_json {
        Some(s) if !s.is_empty() => s,
        _ => return default_retention_ms,
    };
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
