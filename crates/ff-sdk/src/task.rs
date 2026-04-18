use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, Value};
use ff_core::contracts::ReportUsageResult;
use ff_script::error::ScriptError;
use ff_core::keys::{usage_dedup_key, BudgetKeyContext, ExecKeyContext, IndexKeys};
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
        /// HMAC token that signal deliverers must supply to this waitpoint.
        waitpoint_token: WaitpointToken,
    },
    /// Buffered signals already satisfied the condition — suspension skipped.
    /// The caller still holds the lease.
    AlreadySatisfied {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointToken,
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
    /// HMAC token issued when the waitpoint was created. Required for
    /// authenticated signal delivery (RFC-004 §Waitpoint Security).
    pub waitpoint_token: WaitpointToken,
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
    #[allow(clippy::too_many_arguments)]
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
            .map(|k| usage_dedup_key(bctx.hash_tag(), k))
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
    ///
    /// Returns both the waitpoint_id AND the HMAC token required by external
    /// callers to buffer signals against this pending waitpoint
    /// (RFC-004 §Waitpoint Security).
    pub async fn create_pending_waitpoint(
        &self,
        waitpoint_key: &str,
        expires_in_ms: u64,
    ) -> Result<(WaitpointId, WaitpointToken), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        let waitpoint_id = WaitpointId::new();
        let expires_at = TimestampMs::from_millis(TimestampMs::now().0 + expires_in_ms as i64);

        // KEYS (4): exec_core, waitpoint_hash, pending_wp_expiry_zset, hmac_secrets
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.waitpoint(&waitpoint_id),
            idx.pending_waitpoint_expiry(),
            idx.waitpoint_hmac_secrets(),
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

        // Response: {1, "OK", waitpoint_id, waitpoint_key, waitpoint_token}.
        // Extract the token (the waitpoint_id is the one we generated locally).
        let token = extract_pending_waitpoint_token(&raw)?;
        Ok((waitpoint_id, token))
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

        // KEYS (17): exec_core, attempt_record, lease_current, lease_history,
        //            lease_expiry, worker_leases, suspension_current, waitpoint_hash,
        //            waitpoint_signals, suspension_timeout, pending_wp_expiry,
        //            active_index, suspended_index, waitpoint_history, wp_condition,
        //            attempt_timeout, hmac_secrets
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
            idx.waitpoint_hmac_secrets(),                        // 17
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
///
/// The span is named `renew_lease` with target `ff_sdk::task` so bench
/// harnesses can attach a `tracing_subscriber::Layer` and measure
/// renewal count + per-call duration via `on_enter` / `on_exit`
/// without polluting the hot path with additional instrumentation.
/// See `benches/harness/src/bin/long_running.rs` for the consumer.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "renew_lease",
    skip_all,
    fields(execution_id = %execution_id)
)]
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

/// Parse the wire-format result of the `ff_report_usage_and_check` Lua
/// function into a typed [`ReportUsageResult`].
///
/// Standard format: `{1, "OK"}`, `{1, "SOFT_BREACH", dim, current, limit}`,
///                  `{1, "HARD_BREACH", dim, current, limit}`, `{1, "ALREADY_APPLIED"}`.
/// Status code `!= 1` is parsed as a [`ScriptError`] via
/// [`ScriptError::from_code_with_detail`].
///
/// Exposed as `pub` so downstream SDKs that speak the same wire format
/// — notably cairn-fabric's `budget_service::parse_spend_result` — can
/// call this directly instead of re-implementing the parse. Keeping one
/// parser paired with the producer (the Lua function in
/// `lua/ff_report_usage_and_check.lua`) is the defence against silent
/// format drift between producer and consumer.
pub fn parse_report_usage_result(raw: &Value) -> Result<ReportUsageResult, SdkError> {
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
        let detail = usage_field_str(arr, 2);
        return Err(SdkError::Script(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
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
/// Extract the waitpoint_token (field index 4 in the 0-indexed response array)
/// from an `ff_create_pending_waitpoint` reply. Runs `parse_success_result`
/// first so error cases produce the usual typed error, not a parse failure.
fn extract_pending_waitpoint_token(raw: &Value) -> Result<WaitpointToken, SdkError> {
    parse_success_result(raw, "ff_create_pending_waitpoint")?;
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => unreachable!("parse_success_result would have rejected non-array"),
    };
    // Layout: [0]=1, [1]="OK", [2]=waitpoint_id, [3]=waitpoint_key, [4]=waitpoint_token
    let token_str = arr
        .get(4)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            SdkError::Script(ScriptError::Parse(
                "ff_create_pending_waitpoint: missing waitpoint_token in response".into(),
            ))
        })?;
    Ok(WaitpointToken::new(token_str))
}

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
        // Extract error code from index 1 and optional detail from index 2
        // (e.g. `capability_mismatch` ships missing tokens there). Variants
        // that carry a String payload pick the detail up via
        // `from_code_with_detail`; other variants ignore it.
        let field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = field_str(2);

        let script_err = ScriptError::from_code_with_detail(&error_code, &detail)
            .unwrap_or_else(|| {
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
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = err_field_str(2);
        return Err(SdkError::Script(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
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

    // Lua returns: {1, status, suspension_id, waitpoint_id, waitpoint_key, waitpoint_token}
    // The suspension_id/waitpoint_id/waitpoint_key values the worker passed in are
    // authoritative (Lua echoes them); waitpoint_token however is MINTED by Lua and
    // must be read from the response.
    let waitpoint_token = arr
        .get(5)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .map(WaitpointToken::new)
        .ok_or_else(|| {
            SdkError::Script(ScriptError::Parse(
                "ff_suspend_execution: missing waitpoint_token in response".into(),
            ))
        })?;

    if sub_status == "ALREADY_SATISFIED" {
        Ok(SuspendOutcome::AlreadySatisfied {
            suspension_id,
            waitpoint_id,
            waitpoint_key,
            waitpoint_token,
        })
    } else {
        Ok(SuspendOutcome::Suspended {
            suspension_id,
            waitpoint_id,
            waitpoint_key,
            waitpoint_token,
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
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = err_field_str(2);
        return Err(SdkError::Script(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
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
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = err_field_str(2);
        return Err(SdkError::Script(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
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
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = err_field_str(2);
        return Err(SdkError::Script(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
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

// ── Stream read / tail (consumer API, RFC-006 #2) ──

/// Maximum tail block duration accepted by [`tail_stream`]. Mirrors the REST
/// endpoint ceiling so SDK callers can't wedge a connection longer than the
/// server would accept.
pub const MAX_TAIL_BLOCK_MS: u64 = 30_000;

/// Maximum frames per read/tail call. Mirrors
/// `ff_core::contracts::STREAM_READ_HARD_CAP` — re-exported here so SDK
/// callers don't need to import ff-core just to read the bound.
pub use ff_core::contracts::STREAM_READ_HARD_CAP;

/// Result of [`read_stream`] / [`tail_stream`] — frames plus the terminal
/// signal so polling consumers can exit cleanly.
///
/// Re-export of `ff_core::contracts::StreamFrames` for SDK ergonomics.
pub use ff_core::contracts::StreamFrames;

fn validate_stream_read_count(count_limit: u64) -> Result<(), SdkError> {
    if count_limit == 0 {
        return Err(SdkError::Config("count_limit must be >= 1".to_owned()));
    }
    if count_limit > STREAM_READ_HARD_CAP {
        return Err(SdkError::Config(format!(
            "count_limit exceeds STREAM_READ_HARD_CAP ({STREAM_READ_HARD_CAP})"
        )));
    }
    Ok(())
}

/// Read frames from a completed or in-flight attempt's stream.
///
/// `from_id` / `to_id` accept XRANGE special markers (`"-"`, `"+"`) or
/// entry IDs. `count_limit` MUST be in `1..=STREAM_READ_HARD_CAP` —
/// `0` returns [`SdkError::Config`].
///
/// Returns a [`StreamFrames`] including `closed_at`/`closed_reason` so
/// consumers know when the producer has finalized the stream. A
/// never-written attempt and an in-progress stream are indistinguishable
/// here — both present as `frames=[]`, `closed_at=None`.
///
/// Intended for consumers (audit, checkpoint replay) that hold a ferriskey
/// client but are not the lease-holding worker — no lease check is
/// performed.
///
/// # Head-of-line note
///
/// A max-limit XRANGE reply (10_000 frames × ~64 KB each) is a
/// multi-MB reply serialized on one TCP socket. Like [`tail_stream`],
/// calling this on a `client` that is also serving FCALLs stalls those
/// FCALLs behind the reply. The REST server isolates reads on its
/// `tail_client`; direct SDK callers should either use a dedicated
/// client OR paginate through smaller `count_limit` slices.
pub async fn read_stream(
    client: &Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    from_id: &str,
    to_id: &str,
    count_limit: u64,
) -> Result<StreamFrames, SdkError> {
    use ff_core::contracts::{ReadFramesArgs, ReadFramesResult};
    validate_stream_read_count(count_limit)?;

    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let keys = ff_script::functions::stream::StreamOpKeys { ctx: &ctx };

    let args = ReadFramesArgs {
        execution_id: execution_id.clone(),
        attempt_index,
        from_id: from_id.to_owned(),
        to_id: to_id.to_owned(),
        count_limit,
    };

    let ReadFramesResult::Frames(f) =
        ff_script::functions::stream::ff_read_attempt_stream(client, &keys, &args)
            .await
            .map_err(SdkError::Script)?;
    Ok(f)
}

/// Tail a live attempt's stream.
///
/// `last_id` is exclusive — XREAD returns entries with id strictly greater
/// than `last_id`. Pass `"0-0"` to start from the beginning.
///
/// `block_ms == 0` → non-blocking peek. `block_ms > 0` → blocks up to that
/// many ms. Rejects `block_ms > MAX_TAIL_BLOCK_MS` and `count_limit`
/// outside `1..=STREAM_READ_HARD_CAP` with [`SdkError::Config`] to keep
/// SDK and REST ceilings aligned.
///
/// Returns a [`StreamFrames`] including `closed_at`/`closed_reason` —
/// polling consumers should loop until `result.is_closed()` is true, then
/// drain and exit. Timeout with no new frames presents as
/// `frames=[], closed_at=None`.
///
/// # Head-of-line warning — use a dedicated client
///
/// `ferriskey::Client` is a pipelined multiplexed connection; Valkey
/// processes commands FIFO on it. `XREAD BLOCK block_ms` does not yield
/// the read side until a frame arrives or the block elapses. If the
/// `client` you pass here is ALSO used for claims, completes, fails,
/// appends, or any other FCALL, a 30-second tail will stall all those
/// calls for up to 30 seconds.
///
/// **Strongly recommended**: build a separate `ferriskey::Client` for
/// tail callers — mirrors the `Server::tail_client` split that the REST
/// server uses internally (see `crates/ff-server/src/server.rs` and
/// RFC-006 Impl Notes §"Dedicated stream-op connection").
///
/// # Tail parallelism caveat (same mux)
///
/// Even a dedicated tail client is still one multiplexed TCP connection.
/// Valkey processes `XREAD BLOCK` calls FIFO on that one socket, and
/// ferriskey's per-call `request_timeout` starts at future-poll — so
/// two concurrent tails against the same client can time out spuriously:
/// the second call's BLOCK budget elapses while it waits for the first
/// BLOCK to return. The REST server handles this internally with a
/// `tokio::sync::Mutex` that serializes `xread_block` calls, giving
/// each call its full `block_ms` budget at the server.
///
/// **Direct SDK callers that need concurrent tails**: either
///   (1) build ONE `ferriskey::Client` per concurrent tail call (a small
///       pool of clients, rotated by the caller), OR
///   (2) wrap `tail_stream` calls in your own `tokio::sync::Mutex` so
///       only one BLOCK is in flight per client at a time.
/// If you need the REST-side backpressure (429 on contention) and the
/// built-in serializer, go through the
/// `/v1/executions/{eid}/attempts/{idx}/stream/tail` endpoint rather
/// than calling this directly.
///
/// This SDK does not enforce either pattern — the mutex belongs at the
/// application layer, and the connection pool belongs at the SDK
/// caller's DI layer; neither has a structured place inside this
/// helper.
///
/// # Timeout handling
///
/// Blocking calls do not hit ferriskey's default `request_timeout` (5s on
/// the server default). For `XREAD`/`XREADGROUP` with a `BLOCK` argument,
/// ferriskey's `get_request_timeout` returns `BlockingCommand(block_ms +
/// 500ms)`, overriding the client's default per-call. A tail with
/// `block_ms = 30_000` gets a 30_500ms effective transport timeout even if
/// the client was built with a shorter `request_timeout`. No custom client
/// configuration is required for timeout reasons — only for head-of-line
/// isolation above.
pub async fn tail_stream(
    client: &Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    last_id: &str,
    block_ms: u64,
    count_limit: u64,
) -> Result<StreamFrames, SdkError> {
    if block_ms > MAX_TAIL_BLOCK_MS {
        return Err(SdkError::Config(format!(
            "block_ms exceeds {MAX_TAIL_BLOCK_MS}ms ceiling"
        )));
    }
    validate_stream_read_count(count_limit)?;

    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let stream_key = ctx.stream(attempt_index);
    let stream_meta_key = ctx.stream_meta(attempt_index);

    ff_script::stream_tail::xread_block(
        client,
        &stream_key,
        &stream_meta_key,
        last_id,
        block_ms,
        count_limit,
    )
    .await
    .map_err(SdkError::Script)
}

#[cfg(test)]
mod parse_report_usage_result_tests {
    use super::*;

    /// `Value::SimpleString` from a `&str`. `usage_field_str` handles
    /// BulkString and SimpleString uniformly (see
    /// `usage_field_str` — `Value::BulkString(b)` → `String::from_utf8_lossy`,
    /// `Value::SimpleString(s)` → clone). SimpleString avoids a
    /// dev-dependency on `bytes` just for test construction.
    fn s(v: &str) -> Result<Value, ferriskey::Error> {
        Ok(Value::SimpleString(v.to_owned()))
    }

    fn int(n: i64) -> Result<Value, ferriskey::Error> {
        Ok(Value::Int(n))
    }

    fn arr(items: Vec<Result<Value, ferriskey::Error>>) -> Value {
        Value::Array(items)
    }

    #[test]
    fn ok_status() {
        let raw = arr(vec![int(1), s("OK")]);
        assert_eq!(parse_report_usage_result(&raw).unwrap(), ReportUsageResult::Ok);
    }

    #[test]
    fn already_applied_status() {
        let raw = arr(vec![int(1), s("ALREADY_APPLIED")]);
        assert_eq!(
            parse_report_usage_result(&raw).unwrap(),
            ReportUsageResult::AlreadyApplied
        );
    }

    #[test]
    fn soft_breach_status() {
        let raw = arr(vec![int(1), s("SOFT_BREACH"), s("tokens"), s("150"), s("100")]);
        match parse_report_usage_result(&raw).unwrap() {
            ReportUsageResult::SoftBreach { dimension, current_usage, soft_limit } => {
                assert_eq!(dimension, "tokens");
                assert_eq!(current_usage, 150);
                assert_eq!(soft_limit, 100);
            }
            other => panic!("expected SoftBreach, got {other:?}"),
        }
    }

    #[test]
    fn hard_breach_status() {
        let raw = arr(vec![int(1), s("HARD_BREACH"), s("requests"), s("10001"), s("10000")]);
        match parse_report_usage_result(&raw).unwrap() {
            ReportUsageResult::HardBreach { dimension, current_usage, hard_limit } => {
                assert_eq!(dimension, "requests");
                assert_eq!(current_usage, 10001);
                assert_eq!(hard_limit, 10000);
            }
            other => panic!("expected HardBreach, got {other:?}"),
        }
    }

    /// Negative case: non-Array input. Guards against a future Lua refactor
    /// that accidentally returns a bare string/int — the parser must fail
    /// loudly rather than silently succeed or panic.
    #[test]
    fn non_array_input_is_parse_error() {
        let raw = Value::SimpleString("OK".to_owned());
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("expected array"),
            "error should mention expected shape, got: {msg}"
        );
    }

    /// Negative case: Array whose first element isn't an Int status code.
    /// The Lua function's first return slot is always `status_code` (1 on
    /// success, an error code otherwise); a non-Int there is a wire-format
    /// break that must surface as a parse error.
    #[test]
    fn first_element_non_int_is_parse_error() {
        let raw = arr(vec![s("not_an_int"), s("OK")]);
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("int"),
            "error should mention Int status code, got: {msg}"
        );
    }
}
