//! Quota concurrency reconciler.
//!
//! Periodically scans quota partitions to correct drift on concurrency
//! counters and clean expired entries from sliding window ZSETs.
//!
//! Concurrency counters drift because INCR (on lease acquire) and DECR
//! (on lease release) happen on different partitions and are not atomic
//! with each other.
//!
//! Cluster-safe: uses SMEMBERS on indexed SETs instead of SCAN.
//!
//! Reference: RFC-008 §Quota Reconciliation, RFC-010 §6.6

use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::keys;
use ff_core::partition::{Partition, PartitionFamily};

use super::{ScanResult, Scanner};

pub struct QuotaReconciler {
    interval: Duration,
    /// Issue #122: accepted for uniform API; not applied.
    filter: ScannerFilter,
}

impl QuotaReconciler {
    pub fn new(interval: Duration) -> Self {
        Self::with_filter(interval, ScannerFilter::default())
    }

    /// Accepts a [`ScannerFilter`] for uniform construction across
    /// all scanners (issue #122) but **does not apply it**. This
    /// scanner iterates quota policies — not executions — and the
    /// `namespace` / `instance_tag` filter dimensions do not map
    /// onto quota partitions.
    pub fn with_filter(interval: Duration, filter: ScannerFilter) -> Self {
        Self { interval, filter }
    }
}

impl Scanner for QuotaReconciler {
    fn name(&self) -> &'static str {
        "quota_reconciler"
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
            family: PartitionFamily::Quota,
            index: partition,
        };
        let tag = p.hash_tag();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "quota_reconciler: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // Discover quota policies via partition-level index SET (cluster-safe)
        let policies_key = keys::quota_policies_index(&tag);
        let quota_ids: Vec<String> = match client
            .cmd("SMEMBERS")
            .arg(&policies_key)
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "quota_reconciler: SMEMBERS failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if quota_ids.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for qid in &quota_ids {
            match reconcile_one_quota(client, &tag, qid, now_ms).await {
                Ok(true) => processed += 1,
                Ok(false) => {} // nothing to do
                Err(e) => {
                    tracing::warn!(
                        partition,
                        quota_id = qid.as_str(),
                        error = %e,
                        "quota_reconciler: reconcile failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Reconcile one quota policy. Returns Ok(true) if something was cleaned.
async fn reconcile_one_quota(
    client: &ferriskey::Client,
    tag: &str,
    quota_id: &str,
    now_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let mut did_work = false;

    // 1. Read quota definition to find rate-limit window dimensions.
    let def_key = format!("ff:quota:{}:{}", tag, quota_id);
    let window_secs: Option<String> = client
        .cmd("HGET")
        .arg(&def_key)
        .arg("requests_per_window_seconds")
        .execute()
        .await?;

    // 2. Clean expired entries from the requests_per_window sliding window ZSET
    if let Some(ref ws) = window_secs
        && let Ok(secs) = ws.parse::<u64>()
        && secs > 0
    {
        let window_ms = secs * 1000;
        let window_key =
            format!("ff:quota:{}:{}:window:requests_per_window", tag, quota_id);
        let cutoff = now_ms.saturating_sub(window_ms);

        let removed: u32 = client
            .cmd("ZREMRANGEBYSCORE")
            .arg(&window_key)
            .arg("-inf")
            .arg(cutoff.to_string().as_str())
            .execute()
            .await
            .unwrap_or(0);

        if removed > 0 {
            did_work = true;
            tracing::debug!(
                quota_id,
                removed,
                "quota_reconciler: trimmed expired window entries"
            );
        }
    }

    // 3. Reconcile concurrency counter (if quota has concurrency cap)
    //
    // Strategy: read admitted_set (SMEMBERS), check each guard key (EXISTS).
    // If guard expired → SREM from set. Count live = true concurrency.
    // SET counter to live count. No SCAN needed (cluster-safe).
    let concurrency_cap: Option<String> = client
        .cmd("HGET")
        .arg(&def_key)
        .arg("active_concurrency_cap")
        .execute()
        .await?;

    if let Some(ref cap_str) = concurrency_cap
        && let Ok(cap) = cap_str.parse::<u64>()
        && cap > 0
    {
        let counter_key = format!("ff:quota:{}:{}:concurrency", tag, quota_id);
        let admitted_set_key = format!("ff:quota:{}:{}:admitted_set", tag, quota_id);

        // SSCAN the admitted set in batches (instead of unbounded SMEMBERS)
        let mut live_count: u64 = 0;
        let mut cursor = "0".to_string();
        loop {
            let result: ferriskey::Value = client
                .cmd("SSCAN")
                .arg(&admitted_set_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await?;

            let (next_cursor, members) = parse_sscan_response(&result);

            for eid in &members {
                let guard_key = format!("ff:quota:{}:{}:admitted:{}", tag, quota_id, eid);
                let exists: bool = client
                    .exists(&guard_key)
                    .await
                    .unwrap_or(false);
                if exists {
                    live_count += 1;
                } else {
                    // Guard expired — clean up from admitted set
                    let _: () = client
                        .cmd("SREM")
                        .arg(&admitted_set_key)
                        .arg(eid.as_str())
                        .execute()
                        .await
                        .unwrap_or_default();
                }
            }

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        // Read stored counter
        let stored: Option<String> = client
            .cmd("GET")
            .arg(&counter_key)
            .execute()
            .await?;
        let stored_count: i64 = stored
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // Correct if drifted
        if stored_count != live_count as i64 {
            let _: () = client
                .cmd("SET")
                .arg(&counter_key)
                .arg(live_count.to_string().as_str())
                .execute()
                .await?;
            tracing::info!(
                quota_id,
                stored = stored_count,
                actual = live_count,
                "quota_reconciler: corrected concurrency counter drift"
            );
            did_work = true;
        }
    }

    Ok(did_work)
}

/// Parse SSCAN response: [cursor, [member1, member2, ...]]
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
