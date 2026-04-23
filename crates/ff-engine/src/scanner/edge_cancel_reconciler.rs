//! RFC-016 Stage D — sibling-cancel reconciler (Invariant Q6 safety net).
//!
//! Stage C's `edge_cancel_dispatcher` populates
//! `ff:idx:{fp:N}:pending_cancel_groups` atomically inside
//! `ff_resolve_dependency` and drains each tuple via
//! `ff_drain_sibling_cancel_group` after per-sibling cancels land. An
//! engine crash between the SADD and the drain leaves stale tuples in
//! the SET pointing at groups whose cancels already completed.
//!
//! This scanner is a safety net that runs at a slower cadence than the
//! dispatcher (default 10s vs 1s). Per flow partition it enumerates
//! `pending_cancel_groups` directly (no full scan) and calls
//! `ff_reconcile_sibling_cancel_group` per tuple. The Lua function
//! decides atomically:
//!   - `sremmed_stale`    — flag false / edgegroup missing → SREM only.
//!   - `completed_drain`  — flag true + all siblings terminal (drain
//!                          interrupted post-cancels) → HDEL + SREM.
//!   - `no_op`            — flag true + some sibling still running; the
//!                          dispatcher will handle on its next tick.
//!
//! The reconciler MUST NOT fight the dispatcher — `no_op` leaves state
//! untouched so the 1s dispatcher cadence retains ownership of the
//! happy path.

use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{ExecutionId, FlowId};

use super::{ScanResult, Scanner};

/// Max tuples processed per partition per cycle. Matches the dispatcher
/// batch size — the reconciler is strictly bounded by the dispatcher's
/// steady-state backlog plus any crash-recovery residue.
const BATCH_SIZE: usize = 50;

pub struct EdgeCancelReconciler {
    interval: Duration,
    filter: ScannerFilter,
    metrics: std::sync::Arc<ff_observability::Metrics>,
}

impl EdgeCancelReconciler {
    pub fn new(interval: Duration) -> Self {
        Self::with_filter(interval, ScannerFilter::default())
    }

    pub fn with_filter(interval: Duration, filter: ScannerFilter) -> Self {
        Self::with_filter_and_metrics(
            interval,
            filter,
            std::sync::Arc::new(ff_observability::Metrics::new()),
        )
    }

    pub fn with_filter_and_metrics(
        interval: Duration,
        filter: ScannerFilter,
        metrics: std::sync::Arc<ff_observability::Metrics>,
    ) -> Self {
        Self {
            interval,
            filter,
            metrics,
        }
    }
}

impl Scanner for EdgeCancelReconciler {
    fn name(&self) -> &'static str {
        "edge_cancel_reconciler"
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
        let pending_key = fidx.pending_cancel_groups();

        // SRANDMEMBER with count so a pathologically-large SET doesn't
        // starve other partitions. The Lua reconcile call handles the
        // SREM atomically; re-observing the same tuple next cycle is
        // idempotent (`no_op "not_in_set"`).
        let members: Vec<String> = match client
            .cmd("SRANDMEMBER")
            .arg(&pending_key)
            .arg(BATCH_SIZE.to_string().as_str())
            .execute()
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    partition,
                    error = %e,
                    "edge_cancel_reconciler: SRANDMEMBER pending_cancel_groups failed"
                );
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if members.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for member in &members {
            match self
                .reconcile_one_group(client, &p, &pending_key, member)
                .await
            {
                ReconcileOutcome::Acted => processed += 1,
                ReconcileOutcome::NoOp => { /* dispatcher owns it */ }
                ReconcileOutcome::Error => errors += 1,
            }
        }

        ScanResult { processed, errors }
    }
}

enum ReconcileOutcome {
    /// SREM'd stale entry, or completed interrupted drain.
    Acted,
    /// Dispatcher still owns this tuple (siblings non-terminal) — left
    /// alone; not counted as processed.
    NoOp,
    /// Malformed tuple or transient FCALL error.
    Error,
}

impl EdgeCancelReconciler {
    async fn reconcile_one_group(
        &self,
        client: &ferriskey::Client,
        flow_p: &Partition,
        pending_key: &str,
        member: &str,
    ) -> ReconcileOutcome {
        let (flow_id_str, downstream_eid_str) = match member.split_once('|') {
            Some((f, d)) if !f.is_empty() && !d.is_empty() => (f, d),
            _ => {
                tracing::warn!(
                    raw = member,
                    "edge_cancel_reconciler: malformed pending_cancel_groups \
                     member; SREM-ing to avoid poison"
                );
                let _: Result<i64, _> = client
                    .cmd("SREM")
                    .arg(pending_key)
                    .arg(member)
                    .execute()
                    .await;
                return ReconcileOutcome::Error;
            }
        };

        let flow_id = match FlowId::parse(flow_id_str) {
            Ok(id) => id,
            Err(_) => {
                let _: Result<i64, _> = client
                    .cmd("SREM")
                    .arg(pending_key)
                    .arg(member)
                    .execute()
                    .await;
                return ReconcileOutcome::Error;
            }
        };

        let downstream_eid = match ExecutionId::parse(downstream_eid_str) {
            Ok(id) => id,
            Err(_) => {
                let _: Result<i64, _> = client
                    .cmd("SREM")
                    .arg(pending_key)
                    .arg(member)
                    .execute()
                    .await;
                return ReconcileOutcome::Error;
            }
        };

        let fctx = FlowKeyContext::new(flow_p, &flow_id);
        let edgegroup_key = fctx.edgegroup(&downstream_eid);
        let flow_id_s = flow_id.to_string();
        let downstream_s = downstream_eid.to_string();
        let keys = [pending_key, edgegroup_key.as_str()];
        let argv = [flow_id_s.as_str(), downstream_s.as_str()];

        let reply: Result<ferriskey::Value, _> = client
            .fcall(
                "ff_reconcile_sibling_cancel_group",
                &keys,
                &argv,
            )
            .await;

        match reply {
            Ok(val) => match extract_action(&val) {
                Some(action) => {
                    self.metrics.inc_sibling_cancel_reconcile(action);
                    match action {
                        "sremmed_stale" | "completed_drain" => {
                            tracing::debug!(
                                flow_id = %flow_id,
                                downstream = %downstream_eid,
                                action,
                                "edge_cancel_reconciler: action applied"
                            );
                            ReconcileOutcome::Acted
                        }
                        // "no_op"
                        _ => ReconcileOutcome::NoOp,
                    }
                }
                None => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        downstream = %downstream_eid,
                        "edge_cancel_reconciler: unparsable FCALL reply"
                    );
                    ReconcileOutcome::Error
                }
            },
            Err(e) => {
                tracing::warn!(
                    flow_id = %flow_id,
                    downstream = %downstream_eid,
                    error = %e,
                    "edge_cancel_reconciler: FCALL failed; retry next cycle"
                );
                ReconcileOutcome::Error
            }
        }
    }
}

/// Lua returns `{1, "OK", <action>, <detail>}` for success. Pull the
/// action string and coerce to a fixed `&'static str` so the metric
/// cardinality is bounded to 3.
fn extract_action(val: &ferriskey::Value) -> Option<&'static str> {
    let arr = match val {
        ferriskey::Value::Array(a) => a,
        _ => return None,
    };
    let action_result = arr.get(2)?;
    let action_val = action_result.as_ref().ok()?;
    let action = match action_val {
        ferriskey::Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        ferriskey::Value::SimpleString(s) => s.clone(),
        _ => return None,
    };
    match action.as_str() {
        "sremmed_stale" => Some("sremmed_stale"),
        "completed_drain" => Some("completed_drain"),
        "no_op" => Some("no_op"),
        _ => None,
    }
}
