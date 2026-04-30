//! Cancel-flow dispatch reconciler.
//!
//! Safety net for async `cancel_flow` member dispatch. `ff-server`'s
//! `cancel_flow_inner` spawns a background task that cancels each
//! member with bounded transport retries and SREMs from `pending_cancels`
//! on success. If the process crashes between `CancellationScheduled`
//! returning and the dispatch finishing — or a member's cancel hits a
//! permanent error the bounded retry can't recover — the member would
//! otherwise escape cancellation because exec_core lives on `{p:N}`
//! and can't atomically consult `flow_core` on `{fp:N}` during claim.
//!
//! This scanner drains the per-partition `cancel_backlog` ZSET: for
//! each flow whose grace window (score) has elapsed, read
//! `pending_cancels`, fire `ff_cancel_execution` per member, and ack
//! via `ff_ack_cancel_member` to SREM. When the set empties, the ack
//! ZREMs the flow from the backlog.
//!
//! The complete close of the cross-slot claim race (a worker claiming
//! a member between the flow's cancel and the member's own cancel)
//! still requires cross-slot flow-state consultation, which is out of
//! scope. This reconciler shrinks the escape window from "forever" to
//! "one reconciler interval plus grace_ms."

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::contracts::CancelExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{
    execution_partition, Partition, PartitionConfig, PartitionFamily,
};
use ff_core::types::{
    AttemptIndex, CancelSource, ExecutionId, FlowId, LaneId, TimestampMs, WaitpointId,
    WorkerInstanceId,
};

use super::{should_skip_candidate, ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;
const MAX_MEMBERS_PER_FLOW_PER_CYCLE: usize = 500;

pub struct CancelReconciler {
    interval: Duration,
    partition_config: PartitionConfig,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl CancelReconciler {
    pub fn new(interval: Duration, partition_config: PartitionConfig) -> Self {
        Self::with_filter(interval, partition_config, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per member
    /// execution (issue #122). Each member's exec partition is
    /// derived from its `ExecutionId` before the filter HGETs.
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

    /// PR-7b Cluster 1: wire an `EngineBackend` for filter-resolution
    /// reads. FCALL routing is cluster 3 scope.
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

impl Scanner for CancelReconciler {
    fn name(&self) -> &'static str {
        "cancel_reconciler"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn filter(&self) -> &ScannerFilter {
        &self.filter
    }

    /// PR-94: `ff_cancel_backlog_depth` gauge sample — ZCARD of the
    /// per-partition cancel_backlog ZSET. Runs once per partition
    /// per cycle. On error returns `None`; the scanner runner
    /// treats any `None` during a cycle where other partitions
    /// sampled as an invalidation and skips the gauge write
    /// (leaves the prior scrape value in place) so a transient
    /// ZCARD failure on one partition does not cause the gauge to
    /// under-report as an apparent drain.
    async fn sample_backlog_depth(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> Option<u64> {
        let p = Partition {
            family: PartitionFamily::Flow,
            index: partition,
        };
        let fidx = FlowIndexKeys::new(&p);
        let backlog_key = fidx.cancel_backlog();
        let card: Result<Option<u64>, _> = client
            .cmd("ZCARD")
            .arg(&backlog_key)
            .execute()
            .await;
        card.ok().flatten()
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
        let backlog_key = fidx.cancel_backlog();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "cancel_reconciler: TIME failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE -inf now: flows whose grace has elapsed AND that
        // still have owed cancels. Live dispatch SREMs entries as it
        // succeeds, so if the live path finishes before grace expiry
        // the ack removes the flow from the backlog and this scanner
        // sees nothing to do. We only pick up the tail.
        let flow_ids: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&backlog_key)
            .arg("-inf")
            .arg(now_ms.to_string().as_str())
            .arg("LIMIT")
            .arg("0")
            .arg(BATCH_SIZE.to_string().as_str())
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    partition,
                    error = %e,
                    "cancel_reconciler: ZRANGEBYSCORE cancel_backlog failed"
                );
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if flow_ids.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for flow_id_str in flow_ids {
            let flow_id = match FlowId::parse(&flow_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        raw = %flow_id_str,
                        error = %e,
                        "cancel_reconciler: malformed flow_id in cancel_backlog; ZREM"
                    );
                    let _: Result<i64, _> = client
                        .cmd("ZREM")
                        .arg(&backlog_key)
                        .arg(flow_id_str.as_str())
                        .execute()
                        .await;
                    errors += 1;
                    continue;
                }
            };
            let fctx = FlowKeyContext::new(&p, &flow_id);
            let pending_key = fctx.pending_cancels();

            // Defensive: if flow_core is gone (retention, operator-
            // triggered DEL), there's nothing authoritative to ack
            // against. Nuke the backlog entry and move on.
            let core_exists: bool = match client
                .cmd("EXISTS")
                .arg(fctx.core().as_str())
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        error = %e,
                        "cancel_reconciler: EXISTS flow_core failed"
                    );
                    errors += 1;
                    continue;
                }
            };
            if !core_exists {
                let _: Result<i64, _> = client
                    .cmd("DEL")
                    .arg(pending_key.as_str())
                    .execute()
                    .await;
                let _: Result<i64, _> = client
                    .cmd("ZREM")
                    .arg(&backlog_key)
                    .arg(flow_id.to_string().as_str())
                    .execute()
                    .await;
                continue;
            }

            let member_strs: Vec<String> = match client
                .cmd("SRANDMEMBER")
                .arg(pending_key.as_str())
                .arg(MAX_MEMBERS_PER_FLOW_PER_CYCLE.to_string().as_str())
                .execute()
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        error = %e,
                        "cancel_reconciler: SRANDMEMBER pending_cancels failed"
                    );
                    errors += 1;
                    continue;
                }
            };

            if member_strs.is_empty() {
                // Live dispatch drained it before the grace elapsed.
                // The ack path should have ZREMed, but double-check
                // to avoid a stale entry pinning the scanner.
                let _: Result<i64, _> = client
                    .cmd("ZREM")
                    .arg(&backlog_key)
                    .arg(flow_id.to_string().as_str())
                    .execute()
                    .await;
                continue;
            }

            // Re-read the reason from flow_core so the reconciler-
            // initiated cancel carries the original operator intent.
            // On transport error: retry this flow next cycle (don't
            // issue member cancels with a fabricated reason).
            let reason: String = match client
                .cmd("HGET")
                .arg(fctx.core().as_str())
                .arg("cancel_reason")
                .execute::<Option<String>>()
                .await
            {
                Ok(Some(s)) => s,
                Ok(None) => "flow_cancelled".to_owned(),
                Err(e) => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        error = %e,
                        "cancel_reconciler: HGET cancel_reason failed; retry next cycle"
                    );
                    errors += 1;
                    continue;
                }
            };

            for eid_str in &member_strs {
                let execution_id = match ExecutionId::parse(eid_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(
                            flow_id = %flow_id,
                            raw = %eid_str,
                            error = %e,
                            "cancel_reconciler: malformed eid in pending_cancels; SREM"
                        );
                        let _: Result<i64, _> = client
                            .cmd("SREM")
                            .arg(pending_key.as_str())
                            .arg(eid_str.as_str())
                            .execute()
                            .await;
                        errors += 1;
                        continue;
                    }
                };

                // Issue #122: skip members that don't match our filter.
                // Member exec_core may live on a different exec partition
                // than this flow's partition — derive it from the eid.
                let member_part = execution_partition(
                    &execution_id,
                    &self.partition_config,
                ).index;
                if should_skip_candidate(
                    self.backend.as_ref(),
                    &self.filter,
                    member_part,
                    eid_str,
                )
                .await
                {
                    continue;
                }

                if self
                    .cancel_member(client, &execution_id, &reason)
                    .await
                {
                    // ACK: SREM + conditional backlog ZREM as one
                    // atomic unit on the flow partition. Prefer the
                    // trait's `ack_cancel_member` when a backend is
                    // wired; fall back to the direct FCALL otherwise.
                    // A transient ack failure leaves the member in
                    // pending_cancels for next-cycle retry; count it
                    // as an error so scanner stats reflect real
                    // progress (Copilot feedback).
                    let ack_ok = if let Some(backend) = self.backend.as_ref() {
                        match backend.ack_cancel_member(&flow_id, &execution_id).await {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::debug!(
                                    flow_id = %flow_id,
                                    execution_id = %eid_str,
                                    error = %e,
                                    "cancel_reconciler: ack (trait) failed; retry next cycle"
                                );
                                false
                            }
                        }
                    } else {
                        let flow_id_str = flow_id.to_string();
                        let ack_keys = [pending_key.as_str(), backlog_key.as_str()];
                        let ack_args = [eid_str.as_str(), flow_id_str.as_str()];
                        match client
                            .fcall::<ferriskey::Value>("ff_ack_cancel_member", &ack_keys, &ack_args)
                            .await
                        {
                            Ok(_) => true,
                            Err(e) => {
                                tracing::debug!(
                                    flow_id = %flow_id,
                                    execution_id = %eid_str,
                                    error = %e,
                                    "cancel_reconciler: ack failed; retry next cycle"
                                );
                                false
                            }
                        }
                    };
                    if ack_ok {
                        processed += 1;
                    } else {
                        errors += 1;
                    }
                } else {
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

impl CancelReconciler {
    /// PR-7b Cluster 3: prefer the backend trait's
    /// [`EngineBackend::cancel_execution`] when a backend is wired;
    /// fall back to the legacy direct-FCALL path otherwise.
    ///
    /// "Ack-worthy" semantics preserved:
    /// - success: true
    /// - already-terminal (`Contention(ExecutionNotActive)`) /
    ///   `NotFound`: true (the member is gone; retrying is pointless)
    /// - `Validation` / `Bug`: true — these are non-convergent under
    ///   retry. Acking prevents a poison cancel-group member from
    ///   triggering retry storms on every reconcile tick. Emitted at
    ///   `warn` level so operators notice the malformed entry.
    /// - transient transport / other `Contention` / `ResourceExhausted` /
    ///   `State` / `Unavailable`: false (retry next cycle)
    async fn cancel_member(
        &self,
        client: &ferriskey::Client,
        execution_id: &ExecutionId,
        reason: &str,
    ) -> bool {
        if let Some(backend) = self.backend.as_ref() {
            let args = CancelExecutionArgs {
                execution_id: execution_id.clone(),
                reason: reason.to_owned(),
                source: CancelSource::OperatorOverride,
                lease_id: None,
                lease_epoch: None,
                attempt_id: None,
                now: TimestampMs::now(),
            };
            return match backend.cancel_execution(args).await {
                Ok(_) => true,
                // Lua `execution_not_active` maps to
                // `Contention(ExecutionNotActive)` — the member is
                // already terminal; ack to avoid re-issue. All other
                // `Contention` variants are genuinely transient so
                // match only the exact `ExecutionNotActive` kind.
                Err(ff_core::engine_error::EngineError::Contention(
                    ff_core::engine_error::ContentionKind::ExecutionNotActive { .. },
                )) => true,
                // Lua `execution_not_found` → already gone; ack.
                Err(ff_core::engine_error::EngineError::NotFound { .. }) => true,
                // Validation / bug: re-issuing won't converge; ack to
                // avoid poison. Log at warn so operators notice.
                Err(ref e @ ff_core::engine_error::EngineError::Validation { .. })
                | Err(ref e @ ff_core::engine_error::EngineError::Bug(_)) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "cancel_reconciler: non-transient trait error; ack to avoid poison"
                    );
                    true
                }
                // Transport / other contention / resource-exhausted
                // / state / unavailable are transient: leave member
                // for next cycle.
                Err(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "cancel_reconciler: transient trait error; retry next cycle"
                    );
                    false
                }
            };
        }

        cancel_member_fcall(client, &self.partition_config, execution_id, reason).await
    }
}

/// Legacy direct-FCALL path — kept as a fallback for construction
/// paths that do not supply an `EngineBackend` (test harnesses +
/// legacy `new` / `with_filter` call sites). Returns true on
/// "ack-worthy" (success OR already-terminal OR non-retryable
/// transport error we don't want to poison the queue with).
///
/// Mirrors ff-server's `build_cancel_execution_fcall` exactly:
/// pre-reads `lane_id`, `current_attempt_index`, `current_waitpoint_id`,
/// `current_worker_instance_id` from exec_core so the 21-KEY list
/// touches the REAL lane/worker indexes.
async fn cancel_member_fcall(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    reason: &str,
) -> bool {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let idx = IndexKeys::new(&partition);

    // Pre-read dynamic fields. A transient HGET/HMGET failure here
    // surfaces as a retry on the next reconciler pass (return false);
    // we do NOT fall back to defaults, since that would produce the
    // wrong KEYS layout.
    let lane_str: Option<String> = match client.hget(&ctx.core(), "lane_id").await {
        Ok(v) => v,
        Err(e) => {
            let kind = e.kind();
            let retryable = is_retryable_kind(kind);
            if !retryable {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "cancel_reconciler: permanent HGET lane_id error; ack to avoid poison"
                );
                return true;
            }
            tracing::debug!(
                execution_id = %execution_id,
                error = %e,
                "cancel_reconciler: transient HGET lane_id; retry next cycle"
            );
            return false;
        }
    };
    let lane = LaneId::new(lane_str.as_deref().unwrap_or("default"));

    let dyn_fields: Vec<Option<String>> = match client
        .cmd("HMGET")
        .arg(ctx.core())
        .arg("current_attempt_index")
        .arg("current_waitpoint_id")
        .arg("current_worker_instance_id")
        .execute()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let kind = e.kind();
            let retryable = is_retryable_kind(kind);
            if !retryable {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "cancel_reconciler: permanent HMGET error; ack to avoid poison"
                );
                return true;
            }
            tracing::debug!(
                execution_id = %execution_id,
                error = %e,
                "cancel_reconciler: transient HMGET; retry next cycle"
            );
            return false;
        }
    };

    let att_idx_val = dyn_fields
        .first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let att_idx = AttemptIndex::new(att_idx_val);
    let wp_id_str = dyn_fields.get(1).and_then(|v| v.as_ref()).cloned().unwrap_or_default();
    let wp_id = if wp_id_str.is_empty() {
        WaitpointId::new()
    } else {
        WaitpointId::parse(&wp_id_str).unwrap_or_else(|_| WaitpointId::new())
    };
    let wiid_str = dyn_fields.get(2).and_then(|v| v.as_ref()).cloned().unwrap_or_default();
    let wiid = WorkerInstanceId::new(&wiid_str);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        ctx.stream_meta(att_idx),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&wiid),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_condition(&wp_id),
        idx.suspension_timeout(),
        idx.lane_terminal(&lane),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_eligible(&lane),
        idx.lane_delayed(&lane),
        idx.lane_blocked_dependencies(&lane),
        idx.lane_blocked_budget(&lane),
        idx.lane_blocked_quota(&lane),
        idx.lane_blocked_route(&lane),
        idx.lane_blocked_operator(&lane),
    ];
    let argv: Vec<String> = vec![
        execution_id.to_string(),
        reason.to_owned(),
        "operator_override".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    match client.fcall::<ferriskey::Value>("ff_cancel_execution", &kr, &ar).await {
        Ok(ferriskey::Value::Array(arr)) => match arr.first() {
            Some(Ok(ferriskey::Value::Int(1))) => true,
            Some(Ok(ferriskey::Value::Int(0))) => {
                let code = arr
                    .get(1)
                    .and_then(|r| match r {
                        Ok(ferriskey::Value::BulkString(b)) => {
                            Some(String::from_utf8_lossy(b).into_owned())
                        }
                        Ok(ferriskey::Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                matches!(code.as_str(), "execution_not_active" | "execution_not_found")
            }
            _ => false,
        },
        Ok(_) => false,
        Err(e) => {
            let retryable = is_retryable_kind(e.kind());
            if !retryable {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "cancel_reconciler: permanent error on FCALL; ack to avoid poison"
                );
                return true;
            }
            tracing::debug!(
                execution_id = %execution_id,
                error = %e,
                "cancel_reconciler: transient FCALL error; retry next cycle"
            );
            false
        }
    }
}

/// Centralised retry classification so this scanner's policy tracks
/// `ff_script::retry::is_retryable_kind`. Inlined (not imported) because
/// ff-engine doesn't depend on ff-script to keep the scanner crate light;
/// the variants are exhaustively matched with a `_ => false` fallback so
/// a new upstream `ErrorKind` can't silently become retryable.
fn is_retryable_kind(kind: ferriskey::ErrorKind) -> bool {
    use ferriskey::ErrorKind::*;
    matches!(
        kind,
        IoError | FatalSendError | TryAgain | BusyLoadingError | ClusterDown
    )
}
