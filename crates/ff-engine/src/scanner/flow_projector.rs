//! Flow summary projector scanner.
//!
//! Scans flow partitions ({fp:N}). For each flow: delegates the per-flow
//! sample + summary-hash write to `EngineBackend::project_flow_summary`.
//! The engine-side scanner body is thin by design — enumeration of flows
//! is Valkey-specific (SSCAN on an index SET), but the projection itself
//! lives behind the trait.
//!
//! Interval: 15s (catchup mode — event-driven in production).
//!
//! Cluster-safe: uses SSCAN on a partition-level index SET instead of SCAN.
//!
//! # Two sources of `public_flow_state`
//!
//! This scanner writes a DERIVED `public_flow_state` field to the flow
//! summary projection. It does NOT touch `flow_core.public_flow_state`
//! — that field is owned exclusively by `ff_create_flow` and
//! `ff_cancel_flow` and is the authoritative state used for mutation
//! guards (e.g. `ff_add_execution_to_flow` rejects adds when
//! `flow_core.public_flow_state` is terminal).
//!
//! Consumer guidance:
//! - Mutation-guard / authoritative state → read `flow_core.public_flow_state`.
//! - Dashboards / projected rollups → read the summary projection.
//!
//! Reference: RFC-007 §Flow Summary Projection, RFC-010 §6.7

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::FlowIndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily};
use ff_core::types::{FlowId, TimestampMs};

use super::{ScanResult, Scanner};

pub struct FlowProjector {
    interval: Duration,
    partition_config: PartitionConfig,
    /// Issue #122: accepted for uniform API; not applied. See
    /// [`Self::with_filter`] rustdoc.
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl FlowProjector {
    pub fn new(interval: Duration, partition_config: PartitionConfig) -> Self {
        Self::with_filter(interval, partition_config, ScannerFilter::default())
    }

    /// Accepts a [`ScannerFilter`] for uniform construction across
    /// all scanners (issue #122) but **does not apply it**. This
    /// scanner projects flow summaries by aggregating the public
    /// states of many member executions per flow; per-member
    /// filtering would add an HGET per member per flow (N×M), which
    /// does not fit the "no extra HGET to filter" budget set in the
    /// issue #122 design. The flow summary remains a cross-tenant
    /// aggregate in shared-Valkey deployments — consumers that
    /// need per-instance summaries should read exec_core directly
    /// with the filter applied.
    pub fn with_filter(
        interval: Duration,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
    ) -> Self {
        Self {
            interval,
            partition_config,
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 2b-B: wire an `EngineBackend` so the per-flow
    /// projection runs through the trait (Valkey: lifted scanner
    /// body; Postgres: migration 0019 `ff_flow_summary` UPSERT;
    /// SQLite: `Unavailable` per RFC-023).
    pub fn with_filter_and_backend(
        interval: Duration,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            partition_config,
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for FlowProjector {
    fn name(&self) -> &'static str {
        "flow_projector"
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
            family: PartitionFamily::Flow,
            index: partition,
        };
        let fidx = FlowIndexKeys::new(&p);
        let flow_index_key = fidx.flow_index();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "flow_projector: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };
        let now_ts = TimestampMs::from_millis(now_ms as i64);

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;
        let mut cursor = "0".to_string();

        // Stream flow_index in SSCAN batches (COUNT 100) instead of a
        // single SMEMBERS. SMEMBERS on a partition with many flows would
        // materialise every flow_id into one Vec<String> before we could
        // project the first one; SSCAN bounds memory to one batch and
        // keeps the Valkey command's server-side work bounded per call.
        //
        // Flow-id enumeration itself stays on the scanner client — it's
        // a Valkey-shape concern (the `ff:idx:{fp:N}:flow_index` SET
        // doesn't exist on Postgres, which resolves flow membership
        // via the `ff_exec_core_flow_idx` index directly). PR-7b
        // Cluster 2b-B scope: the per-flow projection goes through the
        // trait; the iteration remains Valkey-shaped.
        loop {
            let result: ferriskey::Value = match client
                .cmd("SSCAN")
                .arg(&flow_index_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(partition, error = %e, "flow_projector: SSCAN failed");
                    return ScanResult { processed, errors: errors + 1 };
                }
            };

            let (next_cursor, flow_ids) = parse_sscan_response(&result);

            for fid_str in &flow_ids {
                let res = project_one_flow(
                    client,
                    self.backend.as_ref(),
                    &p,
                    &self.partition_config,
                    fid_str,
                    now_ts,
                )
                .await;
                match res {
                    Ok(true) => processed += 1,
                    Ok(false) => {} // no members or already up-to-date
                    Err(e) => {
                        tracing::warn!(
                            partition,
                            flow_id = fid_str.as_str(),
                            error = %e,
                            "flow_projector: projection failed"
                        );
                        errors += 1;
                    }
                }
            }

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        ScanResult { processed, errors }
    }
}

/// Project one flow through the trait when a backend is wired;
/// otherwise fall back to a direct-client path for test builds that
/// instantiate the scanner without a backend (mirrors cluster-1
/// lease_expiry / cluster-2 reconciler patterns). Returns the
/// trait-shape `Result<bool>` unchanged.
async fn project_one_flow(
    client: &ferriskey::Client,
    backend: Option<&Arc<dyn EngineBackend>>,
    partition: &Partition,
    partition_config: &PartitionConfig,
    fid_str: &str,
    now_ms: TimestampMs,
) -> Result<bool, String> {
    let flow_id = match FlowId::parse(fid_str) {
        Ok(id) => id,
        Err(e) => {
            return Err(format!("malformed flow_id {fid_str:?}: {e}"));
        }
    };

    if let Some(backend_arc) = backend {
        return backend_arc
            .project_flow_summary(*partition, &flow_id, now_ms)
            .await
            .map_err(|e: EngineError| e.to_string());
    }

    // Test-only fallback: direct composition on the scanner client.
    // Mirrors the pre-PR-7b body so existing unit tests continue to
    // pass when instantiated without an EngineBackend.
    project_direct_fallback(client, partition, partition_config, fid_str, now_ms)
        .await
        .map_err(|e| e.to_string())
}

/// Direct-client projection used only when the scanner is
/// constructed without an `EngineBackend`. Matches the
/// pre-PR-7b behaviour closely enough for test fixtures.
async fn project_direct_fallback(
    client: &ferriskey::Client,
    partition: &Partition,
    config: &PartitionConfig,
    fid_str: &str,
    now_ms: TimestampMs,
) -> Result<bool, ferriskey::Error> {
    use std::collections::HashMap;

    const BATCH_SIZE: usize = 50;
    let tag = partition.hash_tag();
    let fidx = FlowIndexKeys::new(partition);
    let flow_index_key = fidx.flow_index();

    let core_key = format!("ff:flow:{}:{}:core", tag, fid_str);
    let members_key = format!("ff:flow:{}:{}:members", tag, fid_str);
    let summary_key = format!("ff:flow:{}:{}:summary", tag, fid_str);

    let core_exists: bool = client.exists(&core_key).await.unwrap_or(true);
    if !core_exists {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(&flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .unwrap_or(None);
        return Ok(false);
    }

    let true_total: u64 = client
        .cmd("SCARD")
        .arg(&members_key)
        .execute()
        .await
        .unwrap_or(0);
    if true_total == 0 {
        return Ok(false);
    }

    let member_eids: Vec<String> = client
        .cmd("SRANDMEMBER")
        .arg(&members_key)
        .arg(BATCH_SIZE.to_string().as_str())
        .execute()
        .await
        .unwrap_or_default();
    if member_eids.is_empty() {
        return Ok(false);
    }

    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut sampled: u32 = 0;
    for eid_str in &member_eids {
        let eid = match ff_core::types::ExecutionId::parse(eid_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let member_partition = ff_core::partition::execution_partition(&eid, config);
        let ctx_tag = member_partition.hash_tag();
        let member_core = format!("ff:exec:{}:{}:core", ctx_tag, eid_str);

        let ps: Option<String> = client
            .cmd("HGET")
            .arg(&member_core)
            .arg("public_state")
            .execute()
            .await
            .unwrap_or(None);
        let state = ps.unwrap_or_else(|| "unknown".to_string());
        *counts.entry(state).or_insert(0) += 1;
        sampled += 1;
    }

    let completed = *counts.get("completed").unwrap_or(&0);
    let skipped = *counts.get("skipped").unwrap_or(&0);
    let failed = *counts.get("failed").unwrap_or(&0);
    let cancelled = *counts.get("cancelled").unwrap_or(&0);
    let expired = *counts.get("expired").unwrap_or(&0);
    let active = *counts.get("active").unwrap_or(&0);
    let suspended = *counts.get("suspended").unwrap_or(&0);
    let waiting = *counts.get("waiting").unwrap_or(&0);
    let delayed = *counts.get("delayed").unwrap_or(&0);
    let rate_limited = *counts.get("rate_limited").unwrap_or(&0);
    let waiting_children = *counts.get("waiting_children").unwrap_or(&0);
    let terminal_count = completed + skipped + failed + cancelled + expired;
    let all_terminal = terminal_count == sampled && sampled > 0;
    let flow_state = if all_terminal {
        if failed > 0 || cancelled > 0 || expired > 0 {
            "failed"
        } else {
            "completed"
        }
    } else if active > 0 {
        "running"
    } else if suspended > 0 || delayed > 0 || rate_limited > 0 || waiting_children > 0 {
        "blocked"
    } else {
        "open"
    };

    let now_s = now_ms.0.to_string();
    let _: () = client
        .cmd("HSET")
        .arg(&summary_key)
        .arg("total_members").arg(true_total.to_string().as_str())
        .arg("sampled_members").arg(sampled.to_string().as_str())
        .arg("members_completed").arg(completed.to_string().as_str())
        .arg("members_failed").arg(failed.to_string().as_str())
        .arg("members_cancelled").arg(cancelled.to_string().as_str())
        .arg("members_expired").arg(expired.to_string().as_str())
        .arg("members_skipped").arg(skipped.to_string().as_str())
        .arg("members_active").arg(active.to_string().as_str())
        .arg("members_suspended").arg(suspended.to_string().as_str())
        .arg("members_waiting").arg(waiting.to_string().as_str())
        .arg("members_delayed").arg(delayed.to_string().as_str())
        .arg("members_rate_limited").arg(rate_limited.to_string().as_str())
        .arg("members_waiting_children").arg(waiting_children.to_string().as_str())
        .arg("public_flow_state").arg(flow_state)
        .arg("last_summary_update_at").arg(now_s.as_str())
        .execute()
        .await?;

    if all_terminal && (sampled as u64) == true_total {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(&flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .unwrap_or(None);
    }

    Ok(true)
}

/// Parse an SSCAN reply `[cursor, [member1, member2, ...]]` into
/// `(cursor, Vec<member>)`. Mirrors the helper in quota_reconciler so
/// both scanners agree on the wire shape.
fn parse_sscan_response(val: &ferriskey::Value) -> (String, Vec<String>) {
    let arr = match val {
        ferriskey::Value::Array(a) if a.len() >= 2 => a,
        _ => return ("0".to_string(), vec![]),
    };

    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return ("0".to_string(), vec![]),
    };

    let mut members = Vec::new();
    match &arr[1] {
        Ok(ferriskey::Value::Array(inner)) => {
            for item in inner {
                if let Ok(ferriskey::Value::BulkString(b)) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        Ok(ferriskey::Value::Set(inner)) => {
            for item in inner {
                if let ferriskey::Value::BulkString(b) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        _ => {}
    }

    (cursor, members)
}
