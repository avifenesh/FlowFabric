//! Index consistency reconciler.
//!
//! SSCAN ff:idx:{p:N}:all_executions for each partition. For each
//! execution, reads exec_core to determine its expected index membership,
//! then verifies the execution appears in the correct scheduling sorted set.
//!
//! Phase 1: log-only. Does not auto-fix inconsistencies.
//! Phase 2+: auto-fix via ff_reconcile_execution_index FCALL.
//!
//! Reference: RFC-010 §6.14

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::LaneId;

use super::{should_skip_candidate, ScanResult, Scanner};

/// How many entries to pull per SSCAN iteration.
const SCAN_COUNT: u32 = 100;

pub struct IndexReconciler {
    interval: Duration,
    /// Lanes to check. Phase 1: just "default".
    lanes: Vec<LaneId>,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl IndexReconciler {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self::with_filter(interval, lanes, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(interval: Duration, lanes: Vec<LaneId>, filter: ScannerFilter) -> Self {
        Self {
            interval,
            lanes,
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 1: wire an `EngineBackend` for filter-resolution
    /// reads via `should_skip_candidate`. FCALL routing for this
    /// scanner is cluster 2b scope.
    pub fn with_filter_and_backend(
        interval: Duration,
        lanes: Vec<LaneId>,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            lanes,
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for IndexReconciler {
    fn name(&self) -> &'static str {
        "index_reconciler"
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
        let idx = IndexKeys::new(&p);
        let all_exec_key = idx.all_executions();

        let mut cursor = "0".to_string();
        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        loop {
            // SSCAN all_executions cursor COUNT scan_count
            let result: ferriskey::Value = match client
                .cmd("SSCAN")
                .arg(&all_exec_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg(SCAN_COUNT.to_string().as_str())
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(partition, error = %e, "index_reconciler: SSCAN failed");
                    return ScanResult { processed, errors: errors + 1 };
                }
            };

            // Parse SSCAN response: [next_cursor, [member1, member2, ...]]
            let (next_cursor, members) = match parse_sscan_response(&result) {
                Some(v) => v,
                None => {
                    tracing::warn!(partition, "index_reconciler: unexpected SSCAN response format");
                    return ScanResult { processed, errors: errors + 1 };
                }
            };

            for eid_str in &members {
                if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str).await {
                    continue;
                }
                match check_execution_index(client, &p, &idx, eid_str, &self.lanes).await {
                    Ok(true) => {} // consistent
                    Ok(false) => {
                        // inconsistency logged inside check_execution_index
                        processed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            partition,
                            execution_id = eid_str.as_str(),
                            error = %e,
                            "index_reconciler: check failed"
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

/// Check whether an execution appears in the expected index set for its state.
/// Returns Ok(true) if consistent, Ok(false) if inconsistency detected.
async fn check_execution_index(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    eid_str: &str,
    _lanes: &[LaneId],
) -> Result<bool, ferriskey::Error> {
    let core_key = format!("ff:exec:{}:{}:core", partition.hash_tag(), eid_str);

    // Read the fields we need for index verification
    let fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&core_key)
        .arg("lifecycle_phase")
        .arg("eligibility_state")
        .arg("ownership_state")
        .arg("lane_id")
        .execute()
        .await?;

    if fields.is_empty() || fields[0].is_none() {
        tracing::warn!(
            partition = partition.index,
            execution_id = eid_str,
            "index_reconciler: execution in all_executions but core hash missing"
        );
        return Ok(false);
    }

    let lifecycle = fields[0].as_deref().unwrap_or("");
    let eligibility = fields[1].as_deref().unwrap_or("");
    let ownership = fields[2].as_deref().unwrap_or("");
    let lane_str = fields[3].as_deref().unwrap_or("default");

    // Determine which index this execution should be in
    let expected_index = match (lifecycle, eligibility, ownership) {
        ("active", _, "leased") => "active",
        ("runnable", "eligible_now", _) => "eligible",
        ("runnable", "not_eligible_until_time", _) => "delayed",
        ("runnable", "blocked_by_dependencies", _) => "blocked:dependencies",
        ("runnable", "blocked_by_budget", _) => "blocked:budget",
        ("runnable", "blocked_by_quota", _) => "blocked:quota",
        ("runnable", "blocked_by_route", _) => "blocked:route",
        ("runnable", "blocked_by_operator", _) => "blocked:operator",
        ("suspended", _, _) => "suspended",
        ("terminal", _, _) => "terminal",
        _ => "unknown",
    };

    if expected_index == "unknown" {
        // Can't determine expected index — may be a transitional state
        return Ok(true);
    }

    // Check membership in the expected index
    let lane = LaneId::new(lane_str);
    let expected_key = match expected_index {
        "active" => idx.lane_active(&lane),
        "eligible" => idx.lane_eligible(&lane),
        "delayed" => idx.lane_delayed(&lane),
        "blocked:dependencies" => idx.lane_blocked_dependencies(&lane),
        "blocked:budget" => idx.lane_blocked_budget(&lane),
        "blocked:quota" => idx.lane_blocked_quota(&lane),
        "blocked:route" => idx.lane_blocked_route(&lane),
        "blocked:operator" => idx.lane_blocked_operator(&lane),
        "suspended" => idx.lane_suspended(&lane),
        "terminal" => idx.lane_terminal(&lane),
        _ => return Ok(true),
    };

    // ZSCORE returns nil if not a member
    let score: Option<String> = client
        .cmd("ZSCORE")
        .arg(&expected_key)
        .arg(eid_str)
        .execute()
        .await?;

    if score.is_none() {
        tracing::warn!(
            partition = partition.index,
            execution_id = eid_str,
            expected_index,
            expected_key = expected_key.as_str(),
            lifecycle,
            eligibility,
            ownership,
            "index_reconciler: execution missing from expected index"
        );
        return Ok(false);
    }

    Ok(true)
}

/// Parse SSCAN response from raw Value.
/// SSCAN returns: Array([cursor_string, Array([member1, member2, ...])])
fn parse_sscan_response(val: &ferriskey::Value) -> Option<(String, Vec<String>)> {
    let arr = match val {
        ferriskey::Value::Array(a) => a,
        _ => return None,
    };
    if arr.len() < 2 {
        return None;
    }

    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return None,
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
        _ => return None,
    };

    Some((cursor, members))
}
