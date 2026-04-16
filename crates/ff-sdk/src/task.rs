use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, Value};
use ff_core::contracts::ReportUsageResult;
use ff_script::error::ScriptError;
use ff_core::keys::{BudgetKeyContext, ExecKeyContext, IndexKeys};
use ff_core::partition::{budget_partition, execution_partition, PartitionConfig};
use ff_core::types::*;
use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

use crate::SdkError;

// ── Phase 3: Suspend/Signal types ──

/// Timeout behavior when a suspension deadline is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimeoutBehavior {
    Fail,
    Cancel,
    Expire,
    AutoResume,
    Escalate,
}

impl TimeoutBehavior {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Fail => "fail",
            Self::Cancel => "cancel",
            Self::Expire => "expire",
            Self::AutoResume => "auto_resume_with_timeout_signal",
            Self::Escalate => "escalate",
        }
    }
}

impl std::fmt::Display for TimeoutBehavior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TimeoutBehavior {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fail" => Ok(Self::Fail),
            "cancel" => Ok(Self::Cancel),
            "expire" => Ok(Self::Expire),
            "auto_resume_with_timeout_signal" | "auto_resume" => Ok(Self::AutoResume),
            "escalate" => Ok(Self::Escalate),
            other => Err(format!("unknown timeout behavior: {other}")),
        }
    }
}

/// A condition matcher for the resume condition.
#[derive(Clone, Debug)]
pub struct ConditionMatcher {
    /// Signal name to match (empty = wildcard).
    pub signal_name: String,
}

/// Outcome of a `suspend()` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuspendOutcome {
    /// Execution is now suspended, waitpoint active.
    Suspended {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
    /// Buffered signals already satisfied the condition — suspension skipped.
    /// The caller still holds the lease.
    AlreadySatisfied {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
}

/// A signal to deliver to a suspended execution's waitpoint.
#[derive(Clone, Debug)]
pub struct Signal {
    pub signal_name: String,
    pub signal_category: String,
    pub payload: Option<Vec<u8>>,
    pub source_type: String,
    pub source_identity: String,
    pub idempotency_key: Option<String>,
}

/// Outcome of `deliver_signal()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalOutcome {
    /// Signal accepted, appended to waitpoint but condition not yet satisfied.
    Accepted { signal_id: SignalId, effect: String },
    /// Signal triggered resume — execution is now runnable.
    TriggeredResume { signal_id: SignalId },
    /// Duplicate signal (idempotency key matched).
    Duplicate { existing_signal_id: String },
}

impl SignalOutcome {
    /// Parse a raw `ff_deliver_signal` FCALL result into a `SignalOutcome`.
    ///
    /// Consuming packages that call `ff_deliver_signal` directly can use this
    /// to interpret the Lua return value without depending on SDK internals.
    pub fn from_fcall_value(raw: &Value) -> Result<Self, SdkError> {
        parse_signal_result(raw)
    }
}

/// Outcome of `append_frame()`.
#[derive(Clone, Debug)]
pub struct AppendFrameOutcome {
    /// Valkey Stream entry ID assigned to this frame.
    pub stream_id: String,
    /// Total frame count in the stream after this append.
    pub frame_count: u64,
}

/// Outcome of a `fail()` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailOutcome {
    /// Retry was scheduled — execution is in delayed backoff.
    RetryScheduled {
        delay_until: TimestampMs,
    },
    /// No retries left — execution is terminal failed.
    TerminalFailed,
}

/// A claimed execution with an active lease. The worker processes this task
/// and must call one of `complete()`, `fail()`, or `cancel()` when done.
///
/// The lease is automatically renewed in the background at `lease_ttl / 3`
/// intervals. Renewal stops when the task is consumed or dropped.
///
/// `complete`, `fail`, and `cancel` consume `self` — this prevents
/// double-complete bugs at the type level.
pub struct ClaimedTask {
    /// Shared Valkey client.
    client: Client,
    /// Partition config for key construction.
    partition_config: PartitionConfig,
    /// Execution identity.
    execution_id: ExecutionId,
    /// Current attempt.
    attempt_index: AttemptIndex,
    attempt_id: AttemptId,
    /// Lease identity.
    lease_id: LeaseId,
    lease_epoch: LeaseEpoch,
    /// Lease timing.
    lease_ttl_ms: u64,
    /// Lane used at claim time.
    lane_id: LaneId,
    /// Worker instance that holds this lease (for index cleanup keys).
    worker_instance_id: WorkerInstanceId,
    /// Execution data.
    input_payload: Vec<u8>,
    execution_kind: String,
    tags: HashMap<String, String>,
    /// Background renewal task handle.
    renewal_handle: JoinHandle<()>,
    /// Signal to stop renewal (used before consuming self).
    renewal_stop: Arc<Notify>,
    /// Consecutive lease renewal failures. Shared with the background renewal
    /// task. Reset to 0 on each successful renewal. Workers should check
    /// `is_lease_healthy()` before committing expensive side effects.
    renewal_failures: Arc<AtomicU32>,
    /// Concurrency permit from the worker's semaphore. Held for the lifetime
    /// of the task; released on complete/fail/cancel/drop.
    _concurrency_permit: Option<OwnedSemaphorePermit>,
}

impl ClaimedTask {
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) fn new(
        client: Client,
        partition_config: PartitionConfig,
        execution_id: ExecutionId,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        lease_ttl_ms: u64,
        lane_id: LaneId,
        worker_instance_id: WorkerInstanceId,
        input_payload: Vec<u8>,
        execution_kind: String,
        tags: HashMap<String, String>,
    ) -> Self {
        let renewal_stop = Arc::new(Notify::new());
        let renewal_failures = Arc::new(AtomicU32::new(0));

        let renewal_handle = spawn_renewal_task(
            client.clone(),
            partition_config,
            execution_id.clone(),
            attempt_index,
            attempt_id.clone(),
            lease_id.clone(),
            lease_epoch,
            lease_ttl_ms,
            renewal_stop.clone(),
            renewal_failures.clone(),
        );

        Self {
            client,
            partition_config,
            execution_id,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            lease_ttl_ms,
            lane_id,
            worker_instance_id,
            input_payload,
            execution_kind,
            tags,
            renewal_handle,
            renewal_stop,
            renewal_failures,
            _concurrency_permit: None,
        }
    }

    /// Attach a concurrency permit from the worker's semaphore.
    /// The permit is held for the task's lifetime and released on drop.
    #[allow(dead_code)]
    pub(crate) fn set_concurrency_permit(&mut self, permit: OwnedSemaphorePermit) {
        self._concurrency_permit = Some(permit);
    }

    // ── Accessors ──

    pub fn execution_id(&self) -> &ExecutionId {
        &self.execution_id
    }

    pub fn attempt_index(&self) -> AttemptIndex {
        self.attempt_index
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    pub fn lease_epoch(&self) -> LeaseEpoch {
        self.lease_epoch
    }

    pub fn input_payload(&self) -> &[u8] {
        &self.input_payload
    }

    pub fn execution_kind(&self) -> &str {
        &self.execution_kind
    }

    pub fn tags(&self) -> &HashMap<String, String> {
        &self.tags
    }

    pub fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }

    /// Check if the lease is likely still valid based on renewal success.
    ///
    /// Returns `false` if 3 or more consecutive renewal attempts have failed.
    /// Workers should check this before committing expensive or irreversible
    /// side effects. A `false` return means Valkey may have already expired
    /// the lease and another worker could be processing this execution.
    pub fn is_lease_healthy(&self) -> bool {
        self.renewal_failures.load(Ordering::Relaxed) < 3
    }

    /// Number of consecutive lease renewal failures since the last success.
    ///
    /// Returns 0 when renewals are working normally. Useful for observability
    /// and custom health policies beyond the default threshold of 3.
    pub fn consecutive_renewal_failures(&self) -> u32 {
        self.renewal_failures.load(Ordering::Relaxed)
    }

    // ── Terminal operations (consume self) ──

    /// Delay the execution until `delay_until`.
    ///
    /// Releases the lease. The execution moves to `delayed` state.
    /// Consumes self — the task cannot be used after delay.
    pub async fn delay_execution(self, delay_until: TimestampMs) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        // KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
        //           lease_expiry_zset, worker_leases, active_index,
        //           delayed_zset, attempt_timeout_zset
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.attempt_hash(self.attempt_index),
            ctx.lease_current(),
            ctx.lease_history(),
            idx.lease_expiry(),
            idx.worker_leases(&self.worker_instance_id),
            idx.lane_active(&self.lane_id),
            idx.lane_delayed(&self.lane_id),
            idx.attempt_timeout(),
        ];

        // ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, delay_until
        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
            delay_until.to_string(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_delay_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_success_result(&raw, "ff_delay_execution")
    }

    /// Move execution to waiting_children state.
    ///
    /// Releases the lease. The execution waits for child dependencies to complete.
    /// Consumes self.
    pub async fn move_to_waiting_children(self) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        // KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
        //           lease_expiry_zset, worker_leases, active_index,
        //           blocked_deps_zset, attempt_timeout_zset
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.attempt_hash(self.attempt_index),
            ctx.lease_current(),
            ctx.lease_history(),
            idx.lease_expiry(),
            idx.worker_leases(&self.worker_instance_id),
            idx.lane_active(&self.lane_id),
            idx.lane_blocked_dependencies(&self.lane_id),
            idx.attempt_timeout(),
        ];

        // ARGV (4): execution_id, lease_id, lease_epoch, attempt_id
        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_move_to_waiting_children", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_success_result(&raw, "ff_move_to_waiting_children")
    }

    /// Complete the execution successfully.
    ///
    /// Calls `ff_complete_execution` via FCALL, then stops lease renewal.
    /// Renewal continues during the FCALL to prevent lease expiry under
    /// network latency — the Lua fences on lease_id+epoch atomically.
    /// Consumes self — the task cannot be used after completion.
    ///
    /// # Connection errors
    ///
    /// If the Valkey connection drops during this call, the returned error
    /// does **not** guarantee the operation failed — the Lua function may
    /// have committed before the response was lost. Callers should treat
    /// connection errors on `complete()` as "possibly succeeded" and verify
    /// the execution state via `get_execution_state()` before retrying.
    pub async fn complete(self, result_payload: Option<Vec<u8>>) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        // KEYS (12): must match lua/execution.lua ff_complete_execution positional order
        // exec_core, attempt_hash, lease_expiry_zset, worker_leases,
        // terminal_zset, lease_current, lease_history, active_index,
        // stream_meta, result_key, attempt_timeout_zset, execution_deadline_zset
        let keys: Vec<String> = vec![
            ctx.core(),                                        // 1  exec_core
            ctx.attempt_hash(self.attempt_index),              // 2  attempt_hash
            idx.lease_expiry(),                                // 3  lease_expiry_zset
            idx.worker_leases(&self.worker_instance_id),       // 4  worker_leases
            idx.lane_terminal(&self.lane_id),                  // 5  terminal_zset
            ctx.lease_current(),                               // 6  lease_current
            ctx.lease_history(),                               // 7  lease_history
            idx.lane_active(&self.lane_id),                    // 8  active_index
            ctx.stream_meta(self.attempt_index),               // 9  stream_meta
            ctx.result(),                                      // 10 result_key
            idx.attempt_timeout(),                             // 11 attempt_timeout_zset
            idx.execution_deadline(),                          // 12 execution_deadline_zset
        ];

        let result_bytes = result_payload.unwrap_or_default();
        let result_str = String::from_utf8_lossy(&result_bytes);

        // ARGV (5): must match lua/execution.lua ff_complete_execution positional order
        // execution_id, lease_id, lease_epoch, attempt_id, result_payload
        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
            result_str.into_owned(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_complete_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_success_result(&raw, "ff_complete_execution")
    }

    /// Fail the execution with a reason and error category.
    ///
    /// If the execution policy allows retries, the engine schedules a retry
    /// (delayed backoff). Otherwise, the execution becomes terminal failed.
    /// Returns [`FailOutcome`] so the caller knows what happened.
    ///
    /// # Connection errors
    ///
    /// If the Valkey connection drops during this call, the returned error
    /// does **not** guarantee the operation failed — the Lua function may
    /// have committed before the response was lost. Callers should treat
    /// connection errors on `fail()` as "possibly succeeded" and verify
    /// the execution state via `get_execution_state()` before retrying.
    pub async fn fail(
        self,
        reason: &str,
        error_category: &str,
    ) -> Result<FailOutcome, SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        // KEYS (12): exec_core, attempt_hash, lease_expiry, worker_leases,
        //            terminal_zset, delayed_zset, lease_current, lease_history,
        //            active_index, stream_meta, attempt_timeout, execution_deadline
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.attempt_hash(self.attempt_index),
            idx.lease_expiry(),
            idx.worker_leases(&self.worker_instance_id),
            idx.lane_terminal(&self.lane_id),
            idx.lane_delayed(&self.lane_id),
            ctx.lease_current(),
            ctx.lease_history(),
            idx.lane_active(&self.lane_id),
            ctx.stream_meta(self.attempt_index),
            idx.attempt_timeout(),
            idx.execution_deadline(),
        ];

        // ARGV (7): eid, lease_id, lease_epoch, attempt_id,
        //           failure_reason, failure_category, retry_policy_json
        let retry_policy_json = self.read_retry_policy_json(&ctx).await?;

        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
            reason.to_owned(),
            error_category.to_owned(),
            retry_policy_json,
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_fail_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_fail_result(&raw)
    }

    /// Cancel the execution.
    ///
    /// Stops lease renewal, then calls `ff_cancel_execution` via FCALL.
    /// Consumes self.
    ///
    /// # Connection errors
    ///
    /// If the Valkey connection drops during this call, the returned error
    /// does **not** guarantee the operation failed — the Lua function may
    /// have committed before the response was lost. Callers should treat
    /// connection errors on `cancel()` as "possibly succeeded" and verify
    /// the execution state via `get_execution_state()` before retrying.
    pub async fn cancel(self, reason: &str) -> Result<(), SdkError> {
        self.cancel_inner(reason).await
    }

    /// Internal cancel implementation (shared by cancel and fail-fallback).
    async fn cancel_inner(self, reason: &str) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        // ff_cancel_execution needs 21 KEYS. The Lua constructs active_index
        // and suspended_key dynamically (C2 fix). KEYS[9]/[10] (waitpoint_hash,
        // wp_condition) need the real waitpoint_id — read from exec_core.
        // If no current_waitpoint_id (not suspended), use a placeholder.
        let wp_id_str: Option<String> = self
            .client
            .hget(&ctx.core(), "current_waitpoint_id")
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "read current_waitpoint_id".into() })?;
        let wp_id = match wp_id_str.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => match WaitpointId::parse(s) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        execution_id = %self.execution_id,
                        raw = %s,
                        error = %e,
                        "corrupt waitpoint_id in exec_core, using placeholder"
                    );
                    WaitpointId::new()
                }
            },
            None => WaitpointId::default(),
        };
        let keys: Vec<String> = vec![
            ctx.core(),                                        // 1
            ctx.attempt_hash(self.attempt_index),              // 2
            ctx.stream_meta(self.attempt_index),               // 3
            ctx.lease_current(),                               // 4
            ctx.lease_history(),                               // 5
            idx.lease_expiry(),                                // 6
            idx.worker_leases(&self.worker_instance_id),       // 7
            ctx.suspension_current(),                          // 8
            ctx.waitpoint(&wp_id),                             // 9
            ctx.waitpoint_condition(&wp_id),                   // 10
            idx.suspension_timeout(),                          // 11
            idx.lane_terminal(&self.lane_id),                  // 12
            idx.attempt_timeout(),                             // 13
            idx.execution_deadline(),                          // 14
            idx.lane_eligible(&self.lane_id),                  // 15
            idx.lane_delayed(&self.lane_id),                   // 16
            idx.lane_blocked_dependencies(&self.lane_id),      // 17
            idx.lane_blocked_budget(&self.lane_id),            // 18
            idx.lane_blocked_quota(&self.lane_id),             // 19
            idx.lane_blocked_route(&self.lane_id),             // 20
            idx.lane_blocked_operator(&self.lane_id),          // 21
        ];

        // ARGV (5): must match lua/execution.lua ff_cancel_execution positional order
        // execution_id, reason, source, lease_id, lease_epoch
        let args: Vec<String> = vec![
            self.execution_id.to_string(),     // 1  execution_id
            reason.to_owned(),                 // 2  reason
            "worker".to_owned(),               // 3  source
            self.lease_id.to_string(),         // 4  lease_id
            self.lease_epoch.to_string(),      // 5  lease_epoch
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_cancel_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_success_result(&raw, "ff_cancel_execution")
    }

    // ── Non-terminal operations ──

    /// Manually renew the lease. Also called by the background renewal task.
    pub async fn renew_lease(&self) -> Result<(), SdkError> {
        renew_lease_inner(
            &self.client,
            &self.partition_config,
            &self.execution_id,
            self.attempt_index,
            &self.attempt_id,
            &self.lease_id,
            self.lease_epoch,
            self.lease_ttl_ms,
        )
        .await
    }

    /// Update progress (pct 0-100 and optional message).
    pub async fn update_progress(&self, pct: u8, message: &str) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);

        // KEYS (1): exec_core
        let keys: Vec<String> = vec![ctx.core()];

        // ARGV (5): execution_id, lease_id, lease_epoch,
        //           progress_pct, progress_message
        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            pct.to_string(),
            message.to_owned(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_update_progress", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        parse_success_result(&raw, "ff_update_progress")
    }

    /// Report usage against a budget and check limits.
    ///
    /// Non-consuming — the worker can report usage multiple times.
    /// `dimensions` is a slice of `(dimension_name, delta)` pairs.
    /// `dedup_key` prevents double-counting on retries (auto-prefixed with budget hash tag).
    pub async fn report_usage(
        &self,
        budget_id: &BudgetId,
        dimensions: &[(&str, u64)],
        dedup_key: Option<&str>,
    ) -> Result<ReportUsageResult, SdkError> {
        let partition = budget_partition(budget_id, &self.partition_config);
        let bctx = BudgetKeyContext::new(&partition, budget_id);

        // KEYS (3): budget_usage, budget_limits, budget_def
        let keys: Vec<String> = vec![bctx.usage(), bctx.limits(), bctx.definition()];

        // ARGV: dim_count, dim_1..dim_N, delta_1..delta_N, now_ms, [dedup_key]
        let now = TimestampMs::now();
        let dim_count = dimensions.len();
        let mut argv: Vec<String> = Vec::with_capacity(3 + dim_count * 2);
        argv.push(dim_count.to_string());
        for (dim, _) in dimensions {
            argv.push((*dim).to_string());
        }
        for (_, delta) in dimensions {
            argv.push(delta.to_string());
        }
        argv.push(now.to_string());
        let dedup_key_val = dedup_key
            .filter(|k| !k.is_empty())
            .map(|k| format!("ff:usagededup:{}:{}", bctx.hash_tag(), k))
            .unwrap_or_default();
        argv.push(dedup_key_val);

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_report_usage_and_check", &key_refs, &argv_refs)
            .await
            .map_err(SdkError::Valkey)?;

        parse_report_usage_result(&raw)
    }

    /// Create a pending waitpoint for future signal delivery.
    ///
    /// Non-consuming — the worker keeps the lease. Signals delivered to the
    /// waitpoint are buffered. When the worker later calls `suspend()` with
    /// `use_pending_waitpoint`, buffered signals may immediately satisfy the
    /// resume condition.
    pub async fn create_pending_waitpoint(
        &self,
        waitpoint_key: &str,
        expires_in_ms: u64,
    ) -> Result<WaitpointId, SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        let waitpoint_id = WaitpointId::new();
        let expires_at = TimestampMs::from_millis(TimestampMs::now().0 + expires_in_ms as i64);

        // KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.waitpoint(&waitpoint_id),
            idx.pending_waitpoint_expiry(),
        ];

        // ARGV (5): execution_id, attempt_index, waitpoint_id, waitpoint_key, expires_at
        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.attempt_index.to_string(),
            waitpoint_id.to_string(),
            waitpoint_key.to_owned(),
            expires_at.to_string(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_create_pending_waitpoint", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        parse_success_result(&raw, "ff_create_pending_waitpoint")?;
        Ok(waitpoint_id)
    }

    // ── Phase 4: Streaming ──

    /// Append a frame to the current attempt's output stream.
    ///
    /// Non-consuming — the worker can append many frames during execution.
    /// The stream is created lazily on the first append.
    pub async fn append_frame(
        &self,
        frame_type: &str,
        payload: &[u8],
        metadata: Option<&str>,
    ) -> Result<AppendFrameOutcome, SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);

        let now = TimestampMs::now();

        // KEYS (3): exec_core, stream_key, stream_meta
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.stream(self.attempt_index),
            ctx.stream_meta(self.attempt_index),
        ];

        let payload_str = String::from_utf8_lossy(payload);

        // ARGV (13): execution_id, attempt_index, lease_id, lease_epoch,
        //            frame_type, ts, payload, encoding, correlation_id,
        //            source, retention_maxlen, attempt_id, max_payload_bytes
        let args: Vec<String> = vec![
            self.execution_id.to_string(),     // 1
            self.attempt_index.to_string(),    // 2
            self.lease_id.to_string(),         // 3
            self.lease_epoch.to_string(),      // 4
            frame_type.to_owned(),             // 5
            now.to_string(),                   // 6 ts
            payload_str.into_owned(),          // 7
            "utf8".to_owned(),                 // 8 encoding
            metadata.unwrap_or("").to_owned(), // 9 correlation_id
            "worker".to_owned(),               // 10 source
            "10000".to_owned(),                // 11 retention_maxlen
            self.attempt_id.to_string(),       // 12
            "65536".to_owned(),                // 13 max_payload_bytes
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_append_frame", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        parse_append_frame_result(&raw)
    }

    // ── Phase 3: Suspend ──

    /// Suspend the execution, releasing the lease and creating a waitpoint.
    ///
    /// The execution transitions to `suspended` and the worker loses ownership.
    /// An external signal matching the condition will resume the execution.
    ///
    /// If `condition_matchers` is empty, a wildcard matcher is created that
    /// matches ANY signal name. To require an explicit operator resume with
    /// no signal match, pass a sentinel name that no real signal will use.
    ///
    /// If buffered signals on a pending waitpoint already satisfy the condition,
    /// returns `AlreadySatisfied` and the lease is NOT released.
    ///
    /// Consumes self — the task cannot be used after suspension.
    pub async fn suspend(
        self,
        reason_code: &str,
        condition_matchers: &[ConditionMatcher],
        timeout_ms: Option<u64>,
        timeout_behavior: TimeoutBehavior,
    ) -> Result<SuspendOutcome, SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        let suspension_id = SuspensionId::new();
        let waitpoint_id = WaitpointId::new();
        // For now, use a simple opaque key (real implementation would use HMAC token)
        let waitpoint_key = format!("wpk:{}", waitpoint_id);

        let timeout_at = timeout_ms.map(|ms| TimestampMs::from_millis(TimestampMs::now().0 + ms as i64));

        // Build resume condition JSON
        let required_signal_names: Vec<&str> = condition_matchers
            .iter()
            .map(|m| m.signal_name.as_str())
            .collect();
        let match_mode = if required_signal_names.len() <= 1 { "any" } else { "all" };
        let resume_condition_json = serde_json::json!({
            "condition_type": "signal_set",
            "required_signal_names": required_signal_names,
            "signal_match_mode": match_mode,
            "minimum_signal_count": 1,
            "timeout_behavior": timeout_behavior.as_str(),
            "allow_operator_override": true,
        }).to_string();

        let resume_policy_json = serde_json::json!({
            "resume_target": "runnable",
            "close_waitpoint_on_resume": true,
            "consume_matched_signals": true,
            "retain_signal_buffer_until_closed": true,
        }).to_string();

        // KEYS (16): exec_core, attempt_record, lease_current, lease_history,
        //            lease_expiry, worker_leases, suspension_current, waitpoint_hash,
        //            waitpoint_signals, suspension_timeout, pending_wp_expiry,
        //            active_index, suspended_index, waitpoint_history, wp_condition,
        //            attempt_timeout
        let keys: Vec<String> = vec![
            ctx.core(),                                          // 1
            ctx.attempt_hash(self.attempt_index),                // 2
            ctx.lease_current(),                                 // 3
            ctx.lease_history(),                                 // 4
            idx.lease_expiry(),                                  // 5
            idx.worker_leases(&self.worker_instance_id),         // 6
            ctx.suspension_current(),                            // 7
            ctx.waitpoint(&waitpoint_id),                        // 8
            ctx.waitpoint_signals(&waitpoint_id),                // 9
            idx.suspension_timeout(),                            // 10
            idx.pending_waitpoint_expiry(),                      // 11
            idx.lane_active(&self.lane_id),                      // 12
            idx.lane_suspended(&self.lane_id),                   // 13
            ctx.waitpoints(),                                    // 14
            ctx.waitpoint_condition(&waitpoint_id),              // 15
            idx.attempt_timeout(),                               // 16
        ];

        // ARGV (17): execution_id, attempt_index, attempt_id, lease_id,
        //            lease_epoch, suspension_id, waitpoint_id, waitpoint_key,
        //            reason_code, requested_by, timeout_at, resume_condition_json,
        //            resume_policy_json, continuation_metadata_pointer,
        //            use_pending_waitpoint, timeout_behavior, lease_history_maxlen
        let args: Vec<String> = vec![
            self.execution_id.to_string(),                          // 1
            self.attempt_index.to_string(),                         // 2
            self.attempt_id.to_string(),                            // 3
            self.lease_id.to_string(),                              // 4
            self.lease_epoch.to_string(),                           // 5
            suspension_id.to_string(),                              // 6
            waitpoint_id.to_string(),                               // 7
            waitpoint_key.clone(),                                  // 8
            reason_code.to_owned(),                                 // 9
            "worker".to_owned(),                                    // 10
            timeout_at.map_or(String::new(), |t| t.to_string()),   // 11
            resume_condition_json,                                  // 12
            resume_policy_json,                                     // 13
            String::new(),                                          // 14 continuation_metadata_ptr
            String::new(),                                          // 15 use_pending_waitpoint
            timeout_behavior.as_str().to_owned(),                   // 16
            "1000".to_owned(),                                      // 17 lease_history_maxlen
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_suspend_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        self.stop_renewal();
        parse_suspend_result(&raw, suspension_id, waitpoint_id, waitpoint_key)
    }

    /// Signal the renewal task to stop.
    fn stop_renewal(&self) {
        self.renewal_stop.notify_one();
    }

    /// Read the retry policy JSON from the execution's policy key.
    async fn read_retry_policy_json(&self, ctx: &ExecKeyContext) -> Result<String, SdkError> {
        let policy_str: Option<String> = self
            .client
            .get(&ctx.policy())
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "read retry policy".into() })?;

        match policy_str {
            Some(json) => {
                match serde_json::from_str::<serde_json::Value>(&json) {
                    Ok(policy) => {
                        if let Some(retry) = policy.get("retry_policy") {
                            return Ok(serde_json::to_string(retry).unwrap_or_default());
                        }
                        Ok(String::new())
                    }
                    Err(e) => {
                        tracing::warn!(
                            execution_id = %self.execution_id,
                            error = %e,
                            "malformed retry policy JSON, treating as no policy"
                        );
                        Ok(String::new())
                    }
                }
            }
            None => Ok(String::new()),
        }
    }
}

impl Drop for ClaimedTask {
    fn drop(&mut self) {
        // Abort the background renewal task on drop.
        // This is a safety net — complete/fail/cancel already stop renewal
        // via notify before consuming self. But if the task is dropped
        // without being consumed (e.g., panic), abort prevents leaked renewals.
        if !self.renewal_handle.is_finished() {
            tracing::warn!(
                execution_id = %self.execution_id,
                "ClaimedTask dropped without terminal operation — lease will expire"
            );
        }
        self.renewal_handle.abort();
    }
}

// ── Lease renewal ──

/// Perform a single lease renewal via FCALL ff_renew_lease.
#[allow(clippy::too_many_arguments)]
async fn renew_lease_inner(
    client: &Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    attempt_id: &AttemptId,
    lease_id: &LeaseId,
    lease_epoch: LeaseEpoch,
    lease_ttl_ms: u64,
) -> Result<(), SdkError> {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let idx = IndexKeys::new(&partition);

    // KEYS: exec_core, lease_current, lease_history, lease_expiry_zset
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
    ];

    // ARGV: execution_id, attempt_index, attempt_id, lease_id, lease_epoch,
    //       lease_ttl_ms, lease_history_grace_ms
    let lease_history_grace_ms = 5000_u64; // 5s grace for cleanup
    let args: Vec<String> = vec![
        execution_id.to_string(),
        attempt_index.to_string(),
        attempt_id.to_string(),
        lease_id.to_string(),
        lease_epoch.to_string(),
        lease_ttl_ms.to_string(),
        lease_history_grace_ms.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = client
        .fcall("ff_renew_lease", &key_refs, &arg_refs)
        .await
        .map_err(SdkError::Valkey)?;

    parse_success_result(&raw, "ff_renew_lease")
}

/// Spawn a background tokio task that renews the lease at `ttl / 3` intervals.
///
/// Stops when:
/// - `stop_signal` is notified (complete/fail/cancel called)
/// - Renewal fails with a terminal error (stale_lease, lease_expired, etc.)
/// - The task handle is aborted (ClaimedTask dropped)
#[allow(clippy::too_many_arguments, dead_code)]
fn spawn_renewal_task(
    client: Client,
    partition_config: PartitionConfig,
    execution_id: ExecutionId,
    attempt_index: AttemptIndex,
    attempt_id: AttemptId,
    lease_id: LeaseId,
    lease_epoch: LeaseEpoch,
    lease_ttl_ms: u64,
    stop_signal: Arc<Notify>,
    failure_counter: Arc<AtomicU32>,
) -> JoinHandle<()> {
    let interval = Duration::from_millis(lease_ttl_ms / 3);

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick — the lease was just acquired.
        tick.tick().await;

        loop {
            tokio::select! {
                _ = stop_signal.notified() => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        "lease renewal stopped by signal"
                    );
                    return;
                }
                _ = tick.tick() => {
                    match renew_lease_inner(
                        &client,
                        &partition_config,
                        &execution_id,
                        attempt_index,
                        &attempt_id,
                        &lease_id,
                        lease_epoch,
                        lease_ttl_ms,
                    )
                    .await
                    {
                        Ok(()) => {
                            failure_counter.store(0, Ordering::Relaxed);
                            tracing::trace!(
                                execution_id = %execution_id,
                                "lease renewed"
                            );
                        }
                        Err(SdkError::Script(ref e)) if is_terminal_renewal_error(e) => {
                            failure_counter.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                "lease renewal failed with terminal error, stopping renewal"
                            );
                            return;
                        }
                        Err(e) => {
                            let count = failure_counter.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                consecutive_failures = count,
                                "lease renewal failed (will retry next interval)"
                            );
                        }
                    }
                }
            }
        }
    })
}

/// Check if a script error means renewal should stop permanently.
#[allow(dead_code)]
fn is_terminal_renewal_error(err: &ScriptError) -> bool {
    matches!(
        err,
        ScriptError::StaleLease
            | ScriptError::LeaseExpired
            | ScriptError::LeaseRevoked
            | ScriptError::ExecutionNotActive
            | ScriptError::ExecutionNotFound
    )
}

// ── FCALL result parsing ──

/// Parse ff_report_usage_and_check result.
/// Standard format: {1, "OK"}, {1, "SOFT_BREACH", dim, current, limit},
///                  {1, "HARD_BREACH", dim, current, limit}, {1, "ALREADY_APPLIED"}
fn parse_report_usage_result(raw: &Value) -> Result<ReportUsageResult, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_report_usage_and_check: expected Array".into(),
            )));
        }
    };
    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_report_usage_and_check: expected Int status code".into(),
            )));
        }
    };
    if status_code != 1 {
        let error_code = usage_field_str(arr, 1);
        return Err(SdkError::Script(
            ScriptError::from_code(&error_code).unwrap_or_else(|| {
                ScriptError::Parse(format!("ff_report_usage_and_check: {error_code}"))
            }),
        ));
    }
    let sub_status = usage_field_str(arr, 1);
    match sub_status.as_str() {
        "OK" => Ok(ReportUsageResult::Ok),
        "ALREADY_APPLIED" => Ok(ReportUsageResult::AlreadyApplied),
        "SOFT_BREACH" => {
            let dim = usage_field_str(arr, 2);
            let current: u64 = usage_field_str(arr, 3).parse().unwrap_or(0);
            let limit: u64 = usage_field_str(arr, 4).parse().unwrap_or(0);
            Ok(ReportUsageResult::SoftBreach { dimension: dim, current_usage: current, soft_limit: limit })
        }
        "HARD_BREACH" => {
            let dim = usage_field_str(arr, 2);
            let current: u64 = usage_field_str(arr, 3).parse().unwrap_or(0);
            let limit: u64 = usage_field_str(arr, 4).parse().unwrap_or(0);
            Ok(ReportUsageResult::HardBreach {
                dimension: dim,
                current_usage: current,
                hard_limit: limit,
            })
        }
        _ => Err(SdkError::Script(ScriptError::Parse(format!(
            "ff_report_usage_and_check: unknown sub-status: {sub_status}"
        )))),
    }
}

fn usage_field_str(arr: &[Result<Value, ferriskey::Error>], index: usize) -> String {
    match arr.get(index) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse a standard {1, "OK", ...} / {0, "error", ...} FCALL result.
pub(crate) fn parse_success_result(raw: &Value, function_name: &str) -> Result<(), SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(format!(
                "{function_name}: expected Array, got non-array"
            ))));
        }
    };

    if arr.is_empty() {
        return Err(SdkError::Script(ScriptError::Parse(format!(
            "{function_name}: empty result array"
        ))));
    }

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(format!(
                "{function_name}: expected Int at index 0"
            ))));
        }
    };

    if status_code == 1 {
        Ok(())
    } else {
        // Extract error code from index 1
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());

        let script_err = ScriptError::from_code(&error_code).unwrap_or_else(|| {
            ScriptError::Parse(format!("{function_name}: unknown error: {error_code}"))
        });

        Err(SdkError::Script(script_err))
    }
}

/// Parse ff_suspend_execution result:
///   ok(suspension_id, waitpoint_id, waitpoint_key)
///   ok_already_satisfied(suspension_id, waitpoint_id, waitpoint_key)
fn parse_suspend_result(
    raw: &Value,
    suspension_id: SuspensionId,
    waitpoint_id: WaitpointId,
    waitpoint_key: String,
) -> Result<SuspendOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_suspend_execution: expected Array".into(),
            )));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_suspend_execution: bad status code".into(),
            )));
        }
    };

    if status_code != 1 {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(SdkError::Script(
            ScriptError::from_code(&error_code).unwrap_or_else(|| {
                ScriptError::Parse(format!("ff_suspend_execution: {error_code}"))
            }),
        ));
    }

    // Check sub-status at index 1
    let sub_status = arr
        .get(1)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    if sub_status == "ALREADY_SATISFIED" {
        Ok(SuspendOutcome::AlreadySatisfied {
            suspension_id,
            waitpoint_id,
            waitpoint_key,
        })
    } else {
        Ok(SuspendOutcome::Suspended {
            suspension_id,
            waitpoint_id,
            waitpoint_key,
        })
    }
}

/// Parse ff_deliver_signal result:
///   ok(signal_id, effect)
///   ok_duplicate(existing_signal_id)
pub(crate) fn parse_signal_result(raw: &Value) -> Result<SignalOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_deliver_signal: expected Array".into(),
            )));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_deliver_signal: bad status code".into(),
            )));
        }
    };

    if status_code != 1 {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(SdkError::Script(
            ScriptError::from_code(&error_code).unwrap_or_else(|| {
                ScriptError::Parse(format!("ff_deliver_signal: {error_code}"))
            }),
        ));
    }

    let sub_status = arr
        .get(1)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    if sub_status == "DUPLICATE" {
        let existing_id = arr
            .get(2)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        return Ok(SignalOutcome::Duplicate {
            existing_signal_id: existing_id,
        });
    }

    // Parse: {1, "OK", signal_id, effect}
    let signal_id_str = arr
        .get(2)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let effect = arr
        .get(3)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let signal_id = SignalId::parse(&signal_id_str).map_err(|e| {
        SdkError::Script(ScriptError::Parse(format!(
            "ff_deliver_signal: invalid signal_id from Lua: {e}"
        )))
    })?;

    if effect == "resume_condition_satisfied" {
        Ok(SignalOutcome::TriggeredResume { signal_id })
    } else {
        Ok(SignalOutcome::Accepted { signal_id, effect })
    }
}

/// Parse ff_append_frame result: ok(entry_id, frame_count)
fn parse_append_frame_result(raw: &Value) -> Result<AppendFrameOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_append_frame: expected Array".into(),
            )));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_append_frame: bad status code".into(),
            )));
        }
    };

    if status_code != 1 {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(SdkError::Script(
            ScriptError::from_code(&error_code).unwrap_or_else(|| {
                ScriptError::Parse(format!("ff_append_frame: {error_code}"))
            }),
        ));
    }

    // ok(entry_id, frame_count)  → arr[2] = entry_id, arr[3] = frame_count
    let stream_id = arr
        .get(2)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let frame_count = arr
        .get(3)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => String::from_utf8_lossy(b).parse::<u64>().ok(),
            Ok(Value::SimpleString(s)) => s.parse::<u64>().ok(),
            Ok(Value::Int(n)) => Some(*n as u64),
            _ => None,
        })
        .unwrap_or(0);

    Ok(AppendFrameOutcome {
        stream_id,
        frame_count,
    })
}

/// Parse ff_fail_execution result:
///   ok("retry_scheduled", delay_until)
///   ok("terminal_failed")
fn parse_fail_result(raw: &Value) -> Result<FailOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_fail_execution: expected Array".into(),
            )));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::Script(ScriptError::Parse(
                "ff_fail_execution: bad status code".into(),
            )));
        }
    };

    if status_code != 1 {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(SdkError::Script(
            ScriptError::from_code(&error_code).unwrap_or_else(|| {
                ScriptError::Parse(format!("ff_fail_execution: {error_code}"))
            }),
        ));
    }

    // Parse sub-status from field[2] (index 2 = first field after status+OK)
    let sub_status = arr
        .get(2)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    match sub_status.as_str() {
        "retry_scheduled" => {
            // Lua returns: ok("retry_scheduled", tostring(delay_until))
            // arr[3] = delay_until
            let delay_str = arr
                .get(3)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let delay_until = delay_str.parse::<i64>().unwrap_or(0);

            Ok(FailOutcome::RetryScheduled {
                delay_until: TimestampMs::from_millis(delay_until),
            })
        }
        "terminal_failed" => Ok(FailOutcome::TerminalFailed),
        _ => Err(SdkError::Script(ScriptError::Parse(format!(
            "ff_fail_execution: unexpected sub-status: {sub_status}"
        )))),
    }
}
