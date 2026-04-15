//! Quota concurrency reconciler.
//!
//! Periodically scans quota partitions to correct drift on concurrency
//! counters and clean expired entries from sliding window ZSETs.
//!
//! Concurrency counters drift because INCR (on lease acquire) and DECR
//! (on lease release) happen on different partitions and are not atomic
//! with each other.
//!
//! Reference: RFC-008 §Quota Reconciliation, RFC-010 §6.6

use std::time::Duration;

use ff_core::partition::{Partition, PartitionFamily};

use super::{ScanResult, Scanner};

pub struct QuotaReconciler {
    interval: Duration,
}

impl QuotaReconciler {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Scanner for QuotaReconciler {
    fn name(&self) -> &'static str {
        "quota_reconciler"
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

        // Discover quota policies on this partition via SCAN
        let quota_ids = match scan_quota_ids(client, &tag).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "quota_reconciler: discovery failed");
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

/// Discover quota policy IDs on a partition via SCAN.
async fn scan_quota_ids(
    client: &ferriskey::Client,
    tag: &str,
) -> Result<Vec<String>, ferriskey::Error> {
    let mut quota_ids = Vec::new();
    let pattern = format!("ff:quota:{}:*", tag);
    let prefix = format!("ff:quota:{}:", tag);
    let mut cursor = "0".to_string();

    loop {
        let result: ferriskey::Value = client
            .cmd("SCAN")
            .arg(&cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg("100")
            .execute()
            .await?;

        let (next_cursor, keys) = parse_scan_result(&result);

        for key in keys {
            // Only take definition keys (no :window:, :concurrency, :admitted: suffix)
            if let Some(rest) = key.strip_prefix(&prefix) && !rest.contains(':') {
                quota_ids.push(rest.to_string());
            }
        }

        cursor = next_cursor;
        if cursor == "0" {
            break;
        }
    }

    Ok(quota_ids)
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
    // RFC-008 §4.3: quota hash stores individual named fields, not a JSON blob.
    // Known dimension: "requests_per_window" with fields:
    //   requests_per_window_seconds → window duration
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
    // The concurrency counter is INCRed on admission (ff_check_admission_and_record)
    // but never DECRed when executions complete/fail/cancel — those run on different
    // partitions ({p:N} vs {q:K}). Without reconciliation, the counter grows
    // monotonically and eventually blocks all new admissions.
    //
    // Fix: count live `admitted:<eid>` guard keys (which have TTL = window_ms)
    // and SET the counter to the actual count of currently-admitted executions.
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

        // Count live admitted:* guard keys for this quota.
        // These keys have TTL = window_ms and auto-expire when the
        // admission window closes. The count of live keys IS the
        // true concurrency count.
        let pattern = format!("ff:quota:{}:{}:admitted:*", tag, quota_id);
        let actual_count = count_keys_by_pattern(client, &pattern).await;

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

        // Correct if drifted (counter > actual live keys)
        if stored_count != actual_count as i64 {
            let _: () = client
                .cmd("SET")
                .arg(&counter_key)
                .arg(actual_count.to_string().as_str())
                .execute()
                .await?;
            tracing::info!(
                quota_id,
                stored = stored_count,
                actual = actual_count,
                "quota_reconciler: corrected concurrency counter drift"
            );
            did_work = true;
        }
    }

    Ok(did_work)
}

/// Count keys matching a pattern via SCAN. Used to count live admitted:* guard keys.
async fn count_keys_by_pattern(
    client: &ferriskey::Client,
    pattern: &str,
) -> u64 {
    let mut count: u64 = 0;
    let mut cursor = "0".to_string();

    loop {
        let result: ferriskey::Value = match client
            .cmd("SCAN")
            .arg(&cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg("100")
            .execute()
            .await
        {
            Ok(v) => v,
            Err(_) => break,
        };

        let (next_cursor, keys) = parse_scan_result(&result);
        count += keys.len() as u64;
        cursor = next_cursor;
        if cursor == "0" {
            break;
        }
    }

    count
}

fn parse_scan_result(value: &ferriskey::Value) -> (String, Vec<String>) {
    match value {
        ferriskey::Value::Array(arr) if arr.len() == 2 => {
            let cursor = match &arr[0] {
                Ok(ferriskey::Value::BulkString(b)) => {
                    String::from_utf8_lossy(b).to_string()
                }
                Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
                _ => "0".to_string(),
            };
            let keys = match &arr[1] {
                Ok(ferriskey::Value::Array(keys)) => keys
                    .iter()
                    .filter_map(|v| match v {
                        Ok(ferriskey::Value::BulkString(b)) => {
                            Some(String::from_utf8_lossy(b).to_string())
                        }
                        Ok(ferriskey::Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            (cursor, keys)
        }
        _ => ("0".to_string(), vec![]),
    }
}
