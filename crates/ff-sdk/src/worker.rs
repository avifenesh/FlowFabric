use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferriskey::{Client, Value};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::PartitionConfig;
use ff_core::types::*;

use crate::config::WorkerConfig;
use crate::task::ClaimedTask;
use crate::SdkError;

/// FlowFabric worker — connects to Valkey, claims executions, and provides
/// the worker-facing API.
///
/// # Usage
///
/// ```rust,ignore
/// use ff_sdk::{FlowFabricWorker, WorkerConfig};
///
/// let config = WorkerConfig::new("valkey://localhost:6379", "w1", "w1-i1", "default", "main");
/// let worker = FlowFabricWorker::connect(config).await?;
///
/// loop {
///     if let Some(task) = worker.claim_next().await? {
///         // Process task...
///         task.complete(Some(b"result".to_vec())).await?;
///     } else {
///         tokio::time::sleep(Duration::from_secs(1)).await;
///     }
/// }
/// ```
pub struct FlowFabricWorker {
    client: Client,
    config: WorkerConfig,
    partition_config: PartitionConfig,
    /// Round-robin lane index for multi-lane claim rotation.
    lane_index: AtomicUsize,
}

impl FlowFabricWorker {
    /// Connect to Valkey and prepare the worker.
    ///
    /// Establishes the ferriskey connection. Does NOT load the FlowFabric
    /// library — that is the server's responsibility (ff-server calls
    /// `ff_script::loader::ensure_library()` on startup). The SDK assumes
    /// the library is already loaded.
    pub async fn connect(config: WorkerConfig) -> Result<Self, SdkError> {
        if config.lanes.is_empty() {
            return Err(SdkError::Config("at least one lane is required".into()));
        }

        let client = Client::connect(&config.valkey_url)
            .await
            .map_err(|e| SdkError::Valkey(format!("failed to connect: {e}")))?;

        // Verify connectivity
        let pong: String = client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| SdkError::Valkey(format!("PING failed: {e}")))?;
        if pong != "PONG" {
            return Err(SdkError::Valkey(format!(
                "unexpected PING response: {pong}"
            )));
        }

        // Use default partition config for now.
        // TODO: Read ff:config:partitions from Valkey (set by ff-server on startup).
        let partition_config = PartitionConfig::default();

        tracing::info!(
            worker_id = %config.worker_id,
            instance_id = %config.worker_instance_id,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "FlowFabricWorker connected"
        );

        Ok(Self {
            client,
            config,
            partition_config,
            lane_index: AtomicUsize::new(0),
        })
    }

    /// Get a reference to the underlying ferriskey client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get the worker config.
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Attempt to claim the next eligible execution.
    ///
    /// Phase 1 simplified claim flow:
    /// 1. Pick a lane (round-robin across configured lanes)
    /// 2. Issue a claim grant via `ff_issue_claim_grant` on the execution's partition
    /// 3. Claim the execution via `ff_claim_execution`
    /// 4. Read execution payload + tags
    /// 5. Return a [`ClaimedTask`] with auto lease renewal
    ///
    /// WARNING: This Phase 1 claim path BYPASSES budget/quota checks that the
    /// production scheduler (ff-scheduler::Scheduler::claim_for_worker) enforces.
    /// In production, workers should receive pre-granted claim tokens from the
    /// scheduler instead of calling claim_next() directly. Budget-blocked
    /// executions may be claimed via this path without enforcement.
    ///
    /// Returns `Ok(None)` if no eligible execution is found.
    /// Returns `Err` on Valkey errors or script failures.
    pub async fn claim_next(&self) -> Result<Option<ClaimedTask>, SdkError> {
        let lane_id = self.next_lane();
        let now = TimestampMs::now();

        // Phase 1: We scan eligible executions directly by reading the eligible
        // ZSET across all execution partitions. In production, the scheduler
        // (ff-scheduler) would handle this. For Phase 1, the SDK does a
        // simplified inline claim.

        // Try each execution partition until we find eligible work.
        for partition_idx in 0..self.partition_config.num_execution_partitions {
            let partition = ff_core::partition::Partition {
                family: ff_core::partition::PartitionFamily::Execution,
                index: partition_idx,
            };
            let idx = IndexKeys::new(&partition);
            let eligible_key = idx.lane_eligible(&lane_id);

            // ZRANGEBYSCORE to get the highest-priority eligible execution.
            // Score format: -(priority * 1_000_000_000_000) + created_at_ms
            // ZRANGEBYSCORE with "-inf" "+inf" LIMIT 0 1 gives lowest score = highest priority.
            let result: Value = self
                .client
                .cmd("ZRANGEBYSCORE")
                .arg(&eligible_key)
                .arg("-inf")
                .arg("+inf")
                .arg("LIMIT")
                .arg("0")
                .arg("1")
                .execute()
                .await
                .map_err(|e| SdkError::Valkey(format!("ZRANGEBYSCORE failed: {e}")))?;

            let execution_id_str = match extract_first_array_string(&result) {
                Some(s) => s,
                None => continue, // No eligible executions on this partition
            };

            let execution_id = ExecutionId::parse(&execution_id_str).map_err(|e| {
                SdkError::Script(ff_core::error::ScriptError::Parse(format!(
                    "bad execution_id in eligible set: {e}"
                )))
            })?;

            // Step 1: Issue claim grant
            let grant_result = self
                .issue_claim_grant(&execution_id, &lane_id, &partition, &idx)
                .await;

            match grant_result {
                Ok(()) => {}
                Err(SdkError::Script(ref e)) if is_retryable_claim_error(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "claim grant failed (retryable), trying next partition"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }

            // Step 2: Claim the execution
            match self
                .claim_execution(&execution_id, &lane_id, &partition, now)
                .await
            {
                Ok(task) => return Ok(Some(task)),
                Err(SdkError::Script(ref e)) if is_retryable_claim_error(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "claim execution failed (retryable), trying next partition"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        // No eligible work found on any partition
        Ok(None)
    }

    /// Issue a claim grant for an execution.
    async fn issue_claim_grant(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        idx: &IndexKeys,
    ) -> Result<(), SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);

        // KEYS (3): exec_core, claim_grant_key, eligible_zset
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.claim_grant(),
            idx.lane_eligible(lane_id),
        ];

        // ARGV (8): eid, worker_id, worker_instance_id, lane_id,
        //           capability_hash, grant_ttl_ms, route_snapshot_json, admission_summary
        let args: Vec<String> = vec![
            execution_id.to_string(),
            self.config.worker_id.to_string(),
            self.config.worker_instance_id.to_string(),
            lane_id.to_string(),
            String::new(), // capability_hash
            "5000".to_owned(), // grant_ttl_ms (5 seconds)
            String::new(), // route_snapshot_json
            String::new(), // admission_summary
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_issue_claim_grant", &key_refs, &arg_refs)
            .await
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        crate::task::parse_success_result(&raw, "ff_issue_claim_grant")
    }

    /// Claim an execution (consumes the grant, creates lease + attempt).
    async fn claim_execution(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        now: TimestampMs,
    ) -> Result<ClaimedTask, SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);
        let idx = IndexKeys::new(partition);

        // Pre-read total_attempt_count from exec_core to derive next attempt index.
        // The Lua uses total_attempt_count as the new index and dynamically builds
        // the attempt key from the hash tag, so KEYS[6-8] are placeholders, but
        // we pass the correct index for documentation/debugging.
        let total_str: Option<String> = self.client
            .cmd("HGET")
            .arg(&ctx.core())
            .arg("total_attempt_count")
            .execute()
            .await
            .unwrap_or(None);
        let next_idx = total_str
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let att_idx = AttemptIndex::new(next_idx);

        let lease_id = LeaseId::new().to_string();
        let attempt_id = AttemptId::new().to_string();
        let renew_before_ms = self.config.lease_ttl_ms * 2 / 3;

        // KEYS (14): must match lua/execution.lua ff_claim_execution positional order
        let keys: Vec<String> = vec![
            ctx.core(),                                    // 1  exec_core
            ctx.claim_grant(),                             // 2  claim_grant
            idx.lane_eligible(lane_id),                    // 3  eligible_zset
            idx.lease_expiry(),                            // 4  lease_expiry_zset
            idx.worker_leases(&self.config.worker_instance_id), // 5  worker_leases
            ctx.attempt_hash(att_idx),                     // 6  attempt_hash (placeholder)
            ctx.attempt_usage(att_idx),                    // 7  attempt_usage (placeholder)
            ctx.attempt_policy(att_idx),                   // 8  attempt_policy (placeholder)
            ctx.attempts(),                                // 9  attempts_zset
            ctx.lease_current(),                           // 10 lease_current
            ctx.lease_history(),                           // 11 lease_history
            idx.lane_active(lane_id),                      // 12 active_index
            idx.attempt_timeout(),                         // 13 attempt_timeout_zset
            idx.execution_deadline(),                      // 14 execution_deadline_zset
        ];

        // ARGV (12): must match lua/execution.lua ff_claim_execution positional order
        let args: Vec<String> = vec![
            execution_id.to_string(),                      // 1  execution_id
            self.config.worker_id.to_string(),             // 2  worker_id
            self.config.worker_instance_id.to_string(),    // 3  worker_instance_id
            lane_id.to_string(),                           // 4  lane
            String::new(),                                 // 5  capability_hash
            lease_id.clone(),                              // 6  lease_id
            self.config.lease_ttl_ms.to_string(),          // 7  lease_ttl_ms
            renew_before_ms.to_string(),                   // 8  renew_before_ms
            attempt_id.clone(),                            // 9  attempt_id
            "{}".to_owned(),                               // 10 attempt_policy_json
            String::new(),                                 // 11 attempt_timeout_ms
            String::new(),                                 // 12 execution_deadline_at
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_claim_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        // Parse claim result: {1, "OK", lease_id, lease_epoch, attempt_index,
        //                      attempt_id, attempt_type, lease_expires_at}
        let arr = match &raw {
            Value::Array(arr) => arr,
            _ => {
                return Err(SdkError::Script(ff_core::error::ScriptError::Parse(
                    "ff_claim_execution: expected Array".into(),
                )));
            }
        };

        let status_code = match arr.first() {
            Some(Ok(Value::Int(n))) => *n,
            _ => {
                return Err(SdkError::Script(ff_core::error::ScriptError::Parse(
                    "ff_claim_execution: bad status code".into(),
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
                ff_core::error::ScriptError::from_code(&error_code).unwrap_or_else(|| {
                    ff_core::error::ScriptError::Parse(format!(
                        "ff_claim_execution: {error_code}"
                    ))
                }),
            ));
        }

        // Extract fields from success response
        let field_str = |idx: usize| -> String {
            arr.get(idx + 2) // skip status_code and "OK"
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        let lease_id = LeaseId::parse(&field_str(0))
            .unwrap_or_else(|_| LeaseId::new());
        let lease_epoch = LeaseEpoch::new(field_str(1).parse().unwrap_or(1));
        let attempt_index = AttemptIndex::new(field_str(2).parse().unwrap_or(0));
        let attempt_id = AttemptId::parse(&field_str(3))
            .unwrap_or_else(|_| AttemptId::new());

        // Read execution payload and metadata
        let (input_payload, execution_kind, tags) = self
            .read_execution_context(execution_id, partition)
            .await?;

        Ok(ClaimedTask::new(
            self.client.clone(),
            self.partition_config,
            execution_id.clone(),
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            self.config.lease_ttl_ms,
            lane_id.clone(),
            self.config.worker_instance_id.clone(),
            input_payload,
            execution_kind,
            tags,
        ))
    }

    /// Read execution payload, kind, and tags after claiming.
    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
        partition: &ff_core::partition::Partition,
    ) -> Result<(Vec<u8>, String, HashMap<String, String>), SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);

        // Read payload
        let payload: Option<String> = self
            .client
            .get(&ctx.payload())
            .await
            .map_err(|e| SdkError::Valkey(format!("GET payload failed: {e}")))?;
        let input_payload = payload.unwrap_or_default().into_bytes();

        // Read execution_kind from core
        let kind: Option<String> = self
            .client
            .hget(&ctx.core(), "execution_kind")
            .await
            .map_err(|e| SdkError::Valkey(format!("HGET execution_kind failed: {e}")))?;
        let execution_kind = kind.unwrap_or_default();

        // Read tags
        let tags: HashMap<String, String> = self
            .client
            .hgetall(&ctx.tags())
            .await
            .unwrap_or_default();

        Ok((input_payload, execution_kind, tags))
    }

    // ── Phase 3: Signal delivery ──

    /// Deliver a signal to a suspended execution's waitpoint.
    ///
    /// The engine atomically records the signal, evaluates the resume condition,
    /// and optionally transitions the execution from `suspended` to `runnable`.
    pub async fn deliver_signal(
        &self,
        execution_id: &ExecutionId,
        waitpoint_id: &WaitpointId,
        signal: crate::task::Signal,
    ) -> Result<crate::task::SignalOutcome, SdkError> {
        let partition = ff_core::partition::execution_partition(execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let idx = IndexKeys::new(&partition);

        let signal_id = ff_core::types::SignalId::new();
        let now = TimestampMs::now();

        // Determine lane for index keys — use first configured lane
        let lane_id = &self.config.lanes[0];

        // KEYS (13): exec_core, wp_condition, wp_signals_stream,
        //            exec_signals_zset, signal_hash, signal_payload,
        //            idem_key, waitpoint_hash, suspension_current,
        //            eligible_zset, suspended_zset, delayed_zset,
        //            suspension_timeout_zset
        let idem_key = if let Some(ref ik) = signal.idempotency_key {
            ctx.signal_dedup(waitpoint_id, ik)
        } else {
            String::new()
        };
        let keys: Vec<String> = vec![
            ctx.core(),                                    // 1
            ctx.waitpoint_condition(waitpoint_id),         // 2
            ctx.waitpoint_signals(waitpoint_id),           // 3
            ctx.exec_signals(),                            // 4
            ctx.signal(&signal_id),                        // 5
            ctx.signal_payload(&signal_id),                // 6
            idem_key,                                      // 7
            ctx.waitpoint(waitpoint_id),                   // 8
            ctx.suspension_current(),                      // 9
            idx.lane_eligible(lane_id),                    // 10
            idx.lane_suspended(lane_id),                   // 11
            idx.lane_delayed(lane_id),                     // 12
            idx.suspension_timeout(),                      // 13
        ];

        let payload_str = signal
            .payload
            .as_ref()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();

        // ARGV (17): signal_id, execution_id, waitpoint_id, signal_name,
        //            signal_category, source_type, source_identity,
        //            payload, payload_encoding, idempotency_key,
        //            correlation_id, target_scope, created_at,
        //            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
        //            max_signals_per_execution
        let args: Vec<String> = vec![
            signal_id.to_string(),                           // 1
            execution_id.to_string(),                        // 2
            waitpoint_id.to_string(),                        // 3
            signal.signal_name,                              // 4
            signal.signal_category,                          // 5
            signal.source_type,                              // 6
            signal.source_identity,                          // 7
            payload_str,                                     // 8
            "json".to_owned(),                               // 9 payload_encoding
            signal.idempotency_key.unwrap_or_default(),      // 10
            String::new(),                                   // 11 correlation_id
            "waitpoint".to_owned(),                          // 12 target_scope
            now.to_string(),                                 // 13 created_at
            "86400000".to_owned(),                           // 14 dedup_ttl_ms
            "0".to_owned(),                                  // 15 resume_delay_ms
            "1000".to_owned(),                               // 16 signal_maxlen
            "10000".to_owned(),                              // 17 max_signals
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_deliver_signal", &key_refs, &arg_refs)
            .await
            .map_err(|e| SdkError::Valkey(e.to_string()))?;

        crate::task::parse_signal_result(&raw)
    }

    /// Pick the next lane in round-robin order.
    fn next_lane(&self) -> LaneId {
        let idx = self.lane_index.fetch_add(1, Ordering::Relaxed) % self.config.lanes.len();
        self.config.lanes[idx].clone()
    }
}

/// Check if a claim error is retryable (should try next candidate).
fn is_retryable_claim_error(err: &ff_core::error::ScriptError) -> bool {
    use ff_core::error::ErrorClass;
    matches!(
        err.class(),
        ErrorClass::Retryable | ErrorClass::Informational
    )
}

/// Extract the first string from a Value::Array result.
fn extract_first_array_string(value: &Value) -> Option<String> {
    match value {
        Value::Array(arr) if !arr.is_empty() => match &arr[0] {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}
