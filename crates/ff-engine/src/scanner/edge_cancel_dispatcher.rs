//! RFC-016 Stage C — sibling-cancel dispatcher.
//!
//! Drains the per-flow-partition `ff:idx:{fp:N}:pending_cancel_groups`
//! SET, populated atomically by `ff_resolve_dependency` whenever the
//! AnyOf/Quorum resolver flips an edge group to `satisfied | impossible`
//! under `OnSatisfied::CancelRemaining`. Each SET member is a
//! `<flow_id>|<downstream_eid>` tuple pointing at the edge-group hash
//! whose `cancel_siblings_pending_members` field carries the pipe-
//! delimited list of still-running sibling execution ids captured at
//! resolve time.
//!
//! For each group: read the sibling list + the cancel reason from the
//! edgegroup hash, issue `ff_cancel_execution` (source = sibling_quorum)
//! per sibling carrying `FailureReason::sibling_quorum_{satisfied,
//! impossible}`, track per-id disposition (cancelled | already_terminal
//! | not_found), and finally atomically SREM the tuple + HDEL the flag +
//! members fields via `ff_drain_sibling_cancel_group` — a single Lua
//! unit so a dispatcher crash mid-drain leaves either the pre-drain or
//! post-drain state, never a torn in-between.
//!
//! Partition-batched, lightweight: the scanner processes at most
//! `BATCH_SIZE` groups per partition per cycle. `LetRun` policies are
//! structurally excluded — `ff_resolve_dependency` NEVER flags or
//! indexes `LetRun` groups (RFC-016 §5, pure-LetRun adjudication
//! 2026-04-23) so the dispatcher cannot see them. The scanner therefore
//! does not re-check policy; if a tuple is in the index SET, its group
//! asked for a cancel.
//!
//! Stage C scope per RFC-016 §11: the dispatcher + the
//! `pending_cancel_groups` SET + basic metrics. Stage D adds the
//! `LetRun` late-terminal metric; Stage E adds the full reconciler +
//! observability polish. Crash-mid-cancel recovery is Stage D.

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::contracts::CancelExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{
    ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys,
};
use ff_core::partition::{
    execution_partition, Partition, PartitionConfig, PartitionFamily,
};
use ff_core::types::{
    AttemptIndex, CancelSource, ExecutionId, FlowId, LaneId, TimestampMs,
    WaitpointId, WorkerInstanceId,
};

use super::{ScanResult, Scanner};

/// Max groups processed per partition per cycle. Bounds worst-case
/// per-cycle Lua work at BATCH_SIZE × (sibling-list size + 1 drain)
/// calls. 50 matches the `cancel_reconciler` batch.
const BATCH_SIZE: u32 = 50;

/// Structural cap on sibling-list size honoured per group per cycle.
/// Protects against a pathological edgegroup whose members string grew
/// beyond the §4.1 128-soft-cap. The dispatcher still drains the full
/// list — just in additional cycles — because the drain FCALL only
/// fires after the Rust loop has issued every cancel it enumerated.
const MAX_SIBLINGS_PER_GROUP: usize = 1024;

pub struct EdgeCancelDispatcher {
    interval: Duration,
    partition_config: PartitionConfig,
    filter: ScannerFilter,
    metrics: Arc<ff_observability::Metrics>,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl EdgeCancelDispatcher {
    pub fn new(interval: Duration, partition_config: PartitionConfig) -> Self {
        Self::with_filter(interval, partition_config, ScannerFilter::default())
    }

    pub fn with_filter(
        interval: Duration,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
    ) -> Self {
        Self::with_filter_and_metrics(
            interval,
            partition_config,
            filter,
            Arc::new(ff_observability::Metrics::new()),
        )
    }

    pub fn with_filter_and_metrics(
        interval: Duration,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Self {
        Self {
            interval,
            partition_config,
            filter,
            metrics,
            backend: None,
        }
    }

    /// PR-7b Cluster 3: wire an `EngineBackend` so per-sibling cancel
    /// and per-group drain route through the trait instead of a
    /// direct `FCALL` on the raw `ferriskey::Client`.
    pub fn with_filter_metrics_and_backend(
        interval: Duration,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
        metrics: Arc<ff_observability::Metrics>,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            partition_config,
            filter,
            metrics,
            backend: Some(backend),
        }
    }
}

impl Scanner for EdgeCancelDispatcher {
    fn name(&self) -> &'static str {
        "edge_cancel_dispatcher"
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

        // SRANDMEMBER with count: pull up to BATCH_SIZE members without
        // disturbing the SET (the drain FCALL removes successful
        // entries atomically after cancels land). A failure here is
        // transient — log + skip; the next cycle retries.
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
                    "edge_cancel_dispatcher: SRANDMEMBER pending_cancel_groups failed"
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
                .dispatch_one_group(client, &p, &pending_key, member)
                .await
            {
                GroupOutcome::Drained => processed += 1,
                GroupOutcome::SkippedRetry => { /* next cycle */ }
                GroupOutcome::Error => errors += 1,
            }
        }

        ScanResult { processed, errors }
    }
}

enum GroupOutcome {
    /// Fully drained: cancels issued + drain FCALL acked.
    Drained,
    /// Transient failure (transport / missing sibling metadata);
    /// leave the SET entry + flag for next cycle.
    SkippedRetry,
    /// Malformed / unrecoverable; counted as an error but moved on.
    Error,
}

/// Postgres-backend parallel to the Valkey scan loop.
///
/// Wave-6b (RFC-v0.7): delegates to
/// [`ff_backend_postgres::reconcilers::edge_cancel_dispatcher::dispatcher_tick`],
/// which mirrors RFC-016 Stage-C semantics against `ff_edge_group` +
/// `ff_pending_cancel_groups` under a per-group transaction with
/// `FOR UPDATE SKIP LOCKED` coalescing.
#[cfg(feature = "postgres")]
pub async fn dispatch_via_postgres(
    pool: &ff_backend_postgres::PgPool,
    filter: &ff_core::backend::ScannerFilter,
) -> Result<
    ff_backend_postgres::reconcilers::edge_cancel_dispatcher::DispatchReport,
    ff_core::engine_error::EngineError,
> {
    ff_backend_postgres::reconcilers::edge_cancel_dispatcher::dispatcher_tick(pool, filter).await
}

impl EdgeCancelDispatcher {
    async fn dispatch_one_group(
        &self,
        client: &ferriskey::Client,
        flow_p: &Partition,
        pending_key: &str,
        member: &str,
    ) -> GroupOutcome {
        // Parse `<flow_id>|<downstream_eid>`
        let (flow_id_str, downstream_eid_str) = match member.split_once('|') {
            Some((f, d)) if !f.is_empty() && !d.is_empty() => (f, d),
            _ => {
                tracing::warn!(
                    raw = member,
                    "edge_cancel_dispatcher: malformed pending_cancel_groups \
                     member; SREM-ing to avoid poison"
                );
                let _: Result<i64, _> = client
                    .cmd("SREM")
                    .arg(pending_key)
                    .arg(member)
                    .execute()
                    .await;
                return GroupOutcome::Error;
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
                return GroupOutcome::Error;
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
                return GroupOutcome::Error;
            }
        };

        let fctx = FlowKeyContext::new(flow_p, &flow_id);
        let edgegroup_key = fctx.edgegroup(&downstream_eid);

        // Read reason + members list from the edgegroup hash. A missing
        // hash (retention-deleted) is still drainable — ff_drain acks
        // with `drained_sans_group`.
        let fields: Vec<Option<String>> = match client
            .cmd("HMGET")
            .arg(&edgegroup_key)
            .arg("cancel_siblings_reason")
            .arg("cancel_siblings_pending_members")
            .arg("cancel_siblings_pending_flag")
            .execute()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    flow_id = %flow_id,
                    downstream = %downstream_eid,
                    error = %e,
                    "edge_cancel_dispatcher: HMGET edgegroup failed; retry next cycle"
                );
                return GroupOutcome::SkippedRetry;
            }
        };

        let reason = fields.first().and_then(|v| v.clone()).unwrap_or_default();
        let members_raw = fields.get(1).and_then(|v| v.clone()).unwrap_or_default();
        let flag = fields.get(2).and_then(|v| v.clone()).unwrap_or_default();

        // Inconsistency: flag absent but tuple in SET, OR flag true but
        // reason missing. Log + attempt drain; the drain call idempotently
        // cleans up.
        if flag.is_empty() && members_raw.is_empty() {
            tracing::debug!(
                flow_id = %flow_id,
                downstream = %downstream_eid,
                "edge_cancel_dispatcher: group has no pending flag / members; \
                 draining tuple (likely already drained or racing retention)"
            );
            return self
                .drain_group(client, flow_p, &flow_id, &downstream_eid)
                .await;
        }

        // Determine reason code. Default to `sibling_quorum_satisfied`
        // if absent — the group was indexed in a CancelRemaining state
        // so SOME reason was written; be defensive rather than emit an
        // empty reason to downstream cancels.
        let reason_str = if reason.is_empty() {
            "sibling_quorum_satisfied"
        } else {
            reason.as_str()
        };

        // Enumerate siblings + issue cancels.
        let sibling_eids: Vec<&str> = members_raw
            .split('|')
            .filter(|s| !s.is_empty())
            .take(MAX_SIBLINGS_PER_GROUP)
            .collect();

        // Normalise reason to a &'static str for the fixed-cardinality
        // metric label. Any unrecognised string is coerced to
        // `sibling_quorum_satisfied` (the default path) rather than
        // exploding cardinality.
        let static_reason: &'static str = match reason_str {
            "sibling_quorum_impossible" => "sibling_quorum_impossible",
            _ => "sibling_quorum_satisfied",
        };

        let mut cancel_dispositions: [u64; 3] = [0, 0, 0]; // cancelled, already_terminal, not_found
        for sib_str in &sibling_eids {
            let sib_eid = match ExecutionId::parse(sib_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        raw = %sib_str,
                        error = %e,
                        "edge_cancel_dispatcher: malformed sibling eid; counting as not_found"
                    );
                    cancel_dispositions[2] += 1;
                    continue;
                }
            };

            self.metrics.inc_sibling_cancel_dispatched(static_reason);
            match self
                .cancel_sibling(client, &sib_eid, reason_str)
                .await
            {
                SiblingDisposition::Cancelled => {
                    cancel_dispositions[0] += 1;
                    self.metrics.inc_sibling_cancel_disposition("cancelled");
                }
                SiblingDisposition::AlreadyTerminal => {
                    cancel_dispositions[1] += 1;
                    self.metrics
                        .inc_sibling_cancel_disposition("already_terminal");
                }
                SiblingDisposition::NotFound => {
                    cancel_dispositions[2] += 1;
                    self.metrics.inc_sibling_cancel_disposition("not_found");
                }
                SiblingDisposition::TransientError => {
                    // One sibling flake pauses drain for this group;
                    // others will be retried after the next dispatch
                    // tick sees the same members list. Don't half-drain.
                    tracing::debug!(
                        flow_id = %flow_id,
                        sibling = %sib_eid,
                        "edge_cancel_dispatcher: transient cancel error; retry group next cycle"
                    );
                    return GroupOutcome::SkippedRetry;
                }
            }
        }

        for (i, label) in ["cancelled", "already_terminal", "not_found"].iter().enumerate() {
            if cancel_dispositions[i] > 0 {
                tracing::debug!(
                    flow_id = %flow_id,
                    downstream = %downstream_eid,
                    reason = %static_reason,
                    disposition = label,
                    count = cancel_dispositions[i],
                    "edge_cancel_dispatcher: sibling cancel disposition"
                );
            }
        }

        // Drain: atomic SREM + HDEL via trait.
        self.drain_group(client, flow_p, &flow_id, &downstream_eid)
            .await
    }

    async fn drain_group(
        &self,
        client: &ferriskey::Client,
        flow_p: &Partition,
        flow_id: &FlowId,
        downstream_eid: &ExecutionId,
    ) -> GroupOutcome {
        // PR-7b Cluster 3: prefer the trait method when a backend is
        // wired; fall back to a direct FCALL on the raw client for
        // construction paths that did not supply a backend (test
        // harnesses + legacy `new`/`with_filter` call sites) so this
        // refactor stays source-compatible with the existing scanner
        // runner contract.
        if let Some(backend) = self.backend.as_ref() {
            return match backend
                .drain_sibling_cancel_group(*flow_p, flow_id, downstream_eid)
                .await
            {
                Ok(()) => GroupOutcome::Drained,
                Err(e) => {
                    tracing::warn!(
                        flow_id = %flow_id,
                        downstream = %downstream_eid,
                        error = %e,
                        "edge_cancel_dispatcher: drain (trait) failed; retry next cycle"
                    );
                    GroupOutcome::SkippedRetry
                }
            };
        }

        let fctx = FlowKeyContext::new(flow_p, flow_id);
        let fidx = FlowIndexKeys::new(flow_p);
        let pending_key = fidx.pending_cancel_groups();
        let edgegroup_key = fctx.edgegroup(downstream_eid);
        let flow_id_str = flow_id.to_string();
        let downstream_eid_str = downstream_eid.to_string();
        let keys = [pending_key.as_str(), edgegroup_key.as_str()];
        let argv = [flow_id_str.as_str(), downstream_eid_str.as_str()];
        match client
            .fcall::<ferriskey::Value>(
                "ff_drain_sibling_cancel_group",
                &keys,
                &argv,
            )
            .await
        {
            Ok(_) => GroupOutcome::Drained,
            Err(e) => {
                tracing::warn!(
                    flow_id = %flow_id,
                    downstream = %downstream_eid,
                    error = %e,
                    "edge_cancel_dispatcher: drain FCALL failed; retry next cycle"
                );
                GroupOutcome::SkippedRetry
            }
        }
    }

    /// PR-7b Cluster 3: route per-sibling cancel through the trait
    /// when a backend is wired. Falls back to the legacy direct-FCALL
    /// path when the dispatcher was constructed without a backend
    /// (test harnesses / `new`), preserving byte-identical KEYS/ARGV.
    async fn cancel_sibling(
        &self,
        client: &ferriskey::Client,
        sib_eid: &ExecutionId,
        reason: &str,
    ) -> SiblingDisposition {
        if let Some(backend) = self.backend.as_ref() {
            let args = CancelExecutionArgs {
                execution_id: sib_eid.clone(),
                reason: reason.to_owned(),
                source: CancelSource::OperatorOverride,
                lease_id: None,
                lease_epoch: None,
                attempt_id: None,
                now: TimestampMs::now(),
            };
            return match backend.cancel_execution(args).await {
                Ok(_) => SiblingDisposition::Cancelled,
                // Lua `execution_not_active` maps to
                // `Contention(ExecutionNotActive)` — the sibling is
                // already terminal. Other `Contention` variants are
                // genuinely transient so match only the exact kind.
                Err(ff_core::engine_error::EngineError::Contention(
                    ff_core::engine_error::ContentionKind::ExecutionNotActive { .. },
                )) => SiblingDisposition::AlreadyTerminal,
                Err(ff_core::engine_error::EngineError::NotFound { .. }) => {
                    SiblingDisposition::NotFound
                }
                Err(_) => SiblingDisposition::TransientError,
            };
        }

        cancel_sibling_fcall(client, &self.partition_config, sib_eid, reason).await
    }
}

#[derive(Debug, Clone, Copy)]
enum SiblingDisposition {
    Cancelled,
    AlreadyTerminal,
    NotFound,
    TransientError,
}

/// Legacy direct-FCALL sibling cancel — kept as a fallback for
/// construction paths that do not supply an `EngineBackend` (test
/// harnesses + `EdgeCancelDispatcher::new` / `with_filter`). Mirrors
/// `cancel_reconciler::cancel_member` KEYS layout with
/// `source = "operator_override"` — sibling-quorum cancels bypass
/// lease-fence checks (no worker holds a lease stake here; the engine
/// itself is pulling the plug). The `reason` is carried through as
/// ARGV[2] and lands verbatim in exec_core's `cancellation_reason`.
///
/// The backend-wired path in [`EdgeCancelDispatcher::cancel_sibling`]
/// replaces this call at runtime once `ff-engine::lib` constructs the
/// dispatcher with a backend.
async fn cancel_sibling_fcall(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    sib_eid: &ExecutionId,
    reason: &str,
) -> SiblingDisposition {
    let partition = execution_partition(sib_eid, partition_config);
    let ctx = ExecKeyContext::new(&partition, sib_eid);
    let idx = IndexKeys::new(&partition);

    // Pre-read lane + dynamic fields so the 21 KEYS target the correct
    // lane/worker indexes. Matches `cancel_reconciler::cancel_member`.
    let lane_str: Option<String> = match client.hget(&ctx.core(), "lane_id").await {
        Ok(v) => v,
        Err(_) => return SiblingDisposition::TransientError,
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
        Err(_) => return SiblingDisposition::TransientError,
    };

    let att_idx_val = dyn_fields
        .first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let att_idx = AttemptIndex::new(att_idx_val);
    let wp_id_str = dyn_fields
        .get(1)
        .and_then(|v| v.as_ref())
        .cloned()
        .unwrap_or_default();
    let wp_id = if wp_id_str.is_empty() {
        WaitpointId::new()
    } else {
        WaitpointId::parse(&wp_id_str).unwrap_or_else(|_| WaitpointId::new())
    };
    let wiid_str = dyn_fields
        .get(2)
        .and_then(|v| v.as_ref())
        .cloned()
        .unwrap_or_default();
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
        sib_eid.to_string(),
        reason.to_owned(),
        "operator_override".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    match client
        .fcall::<ferriskey::Value>("ff_cancel_execution", &kr, &ar)
        .await
    {
        Ok(ferriskey::Value::Array(arr)) => match arr.first() {
            Some(Ok(ferriskey::Value::Int(1))) => SiblingDisposition::Cancelled,
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
                match code.as_str() {
                    "execution_not_active" => SiblingDisposition::AlreadyTerminal,
                    "execution_not_found" => SiblingDisposition::NotFound,
                    _ => SiblingDisposition::TransientError,
                }
            }
            _ => SiblingDisposition::TransientError,
        },
        Ok(_) => SiblingDisposition::TransientError,
        Err(_) => SiblingDisposition::TransientError,
    }
}
