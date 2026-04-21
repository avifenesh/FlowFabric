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

use std::time::Duration;

use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{
    execution_partition, Partition, PartitionConfig, PartitionFamily,
};
use ff_core::types::{AttemptIndex, ExecutionId, FlowId, LaneId, WaitpointId, WorkerInstanceId};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;
const MAX_MEMBERS_PER_FLOW_PER_CYCLE: usize = 500;

pub struct CancelReconciler {
    interval: Duration,
    partition_config: PartitionConfig,
}

impl CancelReconciler {
    pub fn new(interval: Duration, partition_config: PartitionConfig) -> Self {
        Self { interval, partition_config }
    }
}

impl Scanner for CancelReconciler {
    fn name(&self) -> &'static str {
        "cancel_reconciler"
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

            // Re-read the reason + cancelled_at from flow_core so the
            // reconciler-initiated cancel carries the same operator
            // intent as the original cancel_flow call.
            let reason: Option<String> = client
                .cmd("HGET")
                .arg(fctx.core().as_str())
                .arg("cancel_reason")
                .execute()
                .await
                .unwrap_or(None);
            let reason = reason.unwrap_or_else(|| "flow_cancelled".to_owned());

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

                if cancel_member(
                    client,
                    &self.partition_config,
                    &execution_id,
                    &reason,
                )
                .await
                {
                    // ACK via FCALL so SREM + conditional backlog ZREM
                    // are one atomic unit on the flow partition.
                    let flow_id_str = flow_id.to_string();
                    let ack_keys = [pending_key.as_str(), backlog_key.as_str()];
                    let ack_args = [eid_str.as_str(), flow_id_str.as_str()];
                    let _: Result<ferriskey::Value, _> = client
                        .fcall("ff_ack_cancel_member", &ack_keys, &ack_args)
                        .await;
                    processed += 1;
                } else {
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Issue a single operator-override cancel on the member's partition.
/// Returns true on "ack-worthy" (success OR already-terminal OR
/// non-retryable transport error we don't want to poison the queue
/// with). Returns false on transient transport errors; the next
/// reconciler cycle will retry.
///
/// KEYS/ARGV layout mirrors ff-server's `try_cancel_member_once`
/// (source = "operator_override", no lease fence — the flow-level
/// cancel has already terminated the flow, so the member's lease is
/// irrelevant).
async fn cancel_member(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    reason: &str,
) -> bool {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let idx = IndexKeys::new(&partition);
    // Lane is derived from exec_core inside the Lua; "default" here
    // is only used to build KEYS that end up ignored when core says
    // otherwise. ff-server's cancel_member_execution uses the same
    // placeholder pattern.
    let lane_id = LaneId::new("default");

    let wp_id_str: Option<String> = client
        .hget(&ctx.core(), "current_waitpoint_id")
        .await
        .unwrap_or(None);
    let wp_id = match wp_id_str.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => WaitpointId::parse(s).unwrap_or_else(|_| WaitpointId::new()),
        None => WaitpointId::default(),
    };

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new("")),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_condition(&wp_id),
        idx.suspension_timeout(),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_eligible(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.lane_blocked_dependencies(&lane_id),
        idx.lane_blocked_budget(&lane_id),
        idx.lane_blocked_quota(&lane_id),
        idx.lane_blocked_route(&lane_id),
        idx.lane_blocked_operator(&lane_id),
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
                // Error path. execution_not_active / execution_not_found
                // both mean "already terminal" from our POV → ack.
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
            use ferriskey::ErrorKind::*;
            let retryable = matches!(
                e.kind(),
                IoError | FatalSendError | TryAgain | BusyLoadingError | ClusterDown
            );
            if !retryable {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "cancel_reconciler: permanent error; ack to avoid poisoning queue"
                );
                return true;
            }
            tracing::debug!(
                execution_id = %execution_id,
                error = %e,
                "cancel_reconciler: transient error; will retry next cycle"
            );
            false
        }
    }
}
