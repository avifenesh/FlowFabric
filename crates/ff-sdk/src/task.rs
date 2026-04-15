use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, Value};
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
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
    fn as_str(&self) -> &str {
        match self {
            Self::Fail => "fail",
            Self::Cancel => "cancel",
            Self::Expire => "expire",
            Self::AutoResume => "auto_resume_with_timeout_signal",
            Self::Escalate => "escalate",
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
        next_attempt_index: AttemptIndex,
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
    /// Concurrency permit from the worker's semaphore. Held for the lifetime
    /// of the task; released on complete/fail/cancel/drop.
    _concurrency_permit: Option<OwnedSemaphorePermit>,
}

impl ClaimedTask {
    /// Create a new ClaimedTask and start the background lease renewal task.
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
            _concurrency_permit: None,
        }
    }

    /// Attach a concurrency permit from the worker's semaphore.
    /// The permit is held for the task's lifetime and released on drop.
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

    // ── Terminal operations (consume self) ──

    /// Complete the execution successfully.
    ///
    /// Stops lease renewal, then calls `ff_complete_execution` via FCALL.
    /// Consumes self — the task cannot be used after completion.
    pub async fn complete(self, result_payload: Option<Vec<u8>>) -> Result<(), SdkError> {
        self.stop_renewal();

        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        let now = TimestampMs::now();

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
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        parse_success_result(&raw, "ff_complete_execution")
    }

    /// Fail the execution with a reason and error category.
    ///
    /// If the execution policy allows retries, the engine schedules a retry
    /// (delayed backoff). Otherwise, the execution becomes terminal failed.
    /// Returns [`FailOutcome`] so the caller knows what happened.
    pub async fn fail(
        self,
        reason: &str,
        error_category: &str,
    ) -> Result<FailOutcome, SdkError> {
        self.stop_renewal();

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
        let retry_policy_json = self.read_retry_policy_json(&ctx).await;

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
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        parse_fail_result(&raw)
    }

    /// Cancel the execution.
    ///
    /// Stops lease renewal, then calls `ff_cancel_execution` via FCALL.
    /// Consumes self.
    pub async fn cancel(self, reason: &str) -> Result<(), SdkError> {
        self.stop_renewal();
        self.cancel_inner(reason).await
    }

    /// Internal cancel implementation (shared by cancel and fail-fallback).
    async fn cancel_inner(self, reason: &str) -> Result<(), SdkError> {
        let partition = execution_partition(&self.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &self.execution_id);
        let idx = IndexKeys::new(&partition);

        let now = TimestampMs::now();

        // ff_cancel_execution needs 21 KEYS. The Lua constructs active_index
        // and suspended_key dynamically (C2 fix). KEYS[9]/[10] (waitpoint_hash,
        // wp_condition) need the real waitpoint_id — read from exec_core.
        // If no current_waitpoint_id (not suspended), use a placeholder.
        let wp_id_str: Option<String> = self
            .client
            .hget(&ctx.core(), "current_waitpoint_id")
            .await
            .ok()
            .flatten();
        let wp_id = wp_id_str
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| WaitpointId::parse(s).ok())
            .unwrap_or_default();
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

        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            reason.to_owned(),
            "worker".to_owned(),     // source
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
            now.to_string(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_cancel_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

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

        let now = TimestampMs::now();

        // TODO: Replace with typed ff_script wrapper when Step 1.2 is complete.
        let keys: Vec<String> = vec![ctx.core()];

        let args: Vec<String> = vec![
            self.execution_id.to_string(),
            self.lease_id.to_string(),
            self.lease_epoch.to_string(),
            self.attempt_id.to_string(),
            pct.to_string(),
            message.to_owned(),
            now.to_string(),
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_update_progress", &key_refs, &arg_refs)
            .await
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        parse_success_result(&raw, "ff_update_progress")
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
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        parse_append_frame_result(&raw)
    }

    // ── Phase 3: Suspend ──

    /// Suspend the execution, releasing the lease and creating a waitpoint.
    ///
    /// The execution transitions to `suspended` and the worker loses ownership.
    /// An external signal matching the condition will resume the execution.
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
        self.stop_renewal();

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
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        parse_suspend_result(&raw, suspension_id, waitpoint_id, waitpoint_key)
    }

    /// Signal the renewal task to stop.
    fn stop_renewal(&self) {
        self.renewal_stop.notify_one();
    }

    /// Read the retry policy JSON from the execution's policy key.
    async fn read_retry_policy_json(&self, ctx: &ExecKeyContext) -> String {
        let policy_str: Option<String> = self
            .client
            .get(&ctx.policy())
            .await
            .ok()
            .flatten();

        match policy_str {
            Some(json) => {
                // Extract just the retry_policy sub-object
                if let Ok(policy) = serde_json::from_str::<serde_json::Value>(&json)
                    && let Some(retry) = policy.get("retry_policy")
                {
                    return serde_json::to_string(retry).unwrap_or_default();
                }
                String::new()
            }
            None => String::new(),
        }
    }
}

impl Drop for ClaimedTask {
    fn drop(&mut self) {
        // Abort the background renewal task on drop.
        // This is a safety net — complete/fail/cancel already stop renewal
        // via notify before consuming self. But if the task is dropped
        // without being consumed (e.g., panic), abort prevents leaked renewals.
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
        .map_err(|e| SdkError::Valkey(e.to_string()))?;

    parse_success_result(&raw, "ff_renew_lease")
}

/// Spawn a background tokio task that renews the lease at `ttl / 3` intervals.
///
/// Stops when:
/// - `stop_signal` is notified (complete/fail/cancel called)
/// - Renewal fails with a terminal error (stale_lease, lease_expired, etc.)
/// - The task handle is aborted (ClaimedTask dropped)
#[allow(clippy::too_many_arguments)]
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
) -> JoinHandle<()> {
    let interval = Duration::from_millis(lease_ttl_ms / 3);

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
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
                            tracing::trace!(
                                execution_id = %execution_id,
                                "lease renewed"
                            );
                        }
                        Err(SdkError::Script(ref e)) if is_terminal_renewal_error(e) => {
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                "lease renewal failed with terminal error, stopping renewal"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
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

    let signal_id = SignalId::parse(&signal_id_str).unwrap_or_else(|_| SignalId::new());

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
///   ok("retry_scheduled", delay_until, next_attempt_index)
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
            let delay_str = arr
                .get(3)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let delay_until = delay_str.parse::<i64>().unwrap_or(0);

            let next_idx_str = arr
                .get(4)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let next_idx = next_idx_str.parse::<u32>().unwrap_or(1);

            Ok(FailOutcome::RetryScheduled {
                delay_until: TimestampMs::from_millis(delay_until),
                next_attempt_index: AttemptIndex::new(next_idx),
            })
        }
        "terminal_failed" => Ok(FailOutcome::TerminalFailed),
        _ => Err(SdkError::Script(ScriptError::Parse(format!(
            "ff_fail_execution: unexpected sub-status: {sub_status}"
        )))),
    }
}
