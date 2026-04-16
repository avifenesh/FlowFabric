//! Claim-grant cycle: find eligible executions and issue grants.
//!
//! The scheduler selects candidates from partition-local eligible sorted sets,
//! then atomically issues a claim grant via `FCALL ff_issue_claim_grant`.
//! The worker (ff-sdk) subsequently consumes the grant via `ff_claim_execution`.
//!
//! Phase 5: single lane, budget/quota pre-checks before grant issuance.
//! Candidates that fail budget/quota are blocked via ff_block_execution_for_admission.
//!
//! Reference: RFC-009 §12.7, RFC-010 §3.1, RFC-008 §1.7

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, budget_partition, quota_partition};
use ff_core::types::{BudgetId, ExecutionId, LaneId, QuotaPolicyId, TimestampMs, WorkerId, WorkerInstanceId};

/// A claim grant issued by the scheduler for a specific execution.
///
/// The worker uses this to call `ff_claim_execution` (or `ff_acquire_lease`)
/// which atomically consumes the grant and creates the lease.
#[derive(Debug)]
pub struct ClaimGrant {
    /// The execution that was granted.
    pub execution_id: ExecutionId,
    /// The partition where this execution lives.
    pub partition: Partition,
    /// The Valkey key holding the grant hash (for the worker to reference).
    pub grant_key: String,
    /// When the grant expires if not consumed.
    pub expires_at_ms: u64,
}

/// Budget check result from a cross-partition budget read.
#[derive(Debug)]
pub enum BudgetCheckResult {
    /// Budget is within limits — proceed.
    Ok,
    /// Budget hard limit breached. Contains (dimension, detail_string).
    HardBreach { dimension: String, detail: String },
}

/// Cross-partition budget checker with per-cycle caching.
///
/// Reads budget usage/limits once per scan cycle (not per candidate).
/// This is MANDATORY for performance — without it, 50K blocked executions
/// would produce 50K budget reads per cycle.
pub struct BudgetChecker {
    /// Cached budget status: budget_id → BudgetCheckResult.
    /// Reset at the start of each scheduler cycle.
    cache: std::collections::HashMap<String, BudgetCheckResult>,
    config: PartitionConfig,
}

impl BudgetChecker {
    pub fn new(config: PartitionConfig) -> Self {
        Self {
            cache: std::collections::HashMap::new(),
            config,
        }
    }

    /// Check a budget by ID. Reads from Valkey on first call per budget,
    /// caches for subsequent candidates in the same cycle.
    pub async fn check_budget(
        &mut self,
        client: &ferriskey::Client,
        budget_id: &str,
    ) -> &BudgetCheckResult {
        if self.cache.contains_key(budget_id) {
            return &self.cache[budget_id];
        }

        // Compute real {b:M} partition tag from budget_id
        let (usage_key, limits_key) = match BudgetId::parse(budget_id) {
            Ok(bid) => {
                let partition = budget_partition(&bid, &self.config);
                let tag = partition.hash_tag();
                (
                    format!("ff:budget:{}:{}:usage", tag, budget_id),
                    format!("ff:budget:{}:{}:limits", tag, budget_id),
                )
            }
            Err(_) => {
                // Fallback for non-UUID budget IDs (test compat)
                (
                    format!("ff:budget:{{b:0}}:{}:usage", budget_id),
                    format!("ff:budget:{{b:0}}:{}:limits", budget_id),
                )
            }
        };

        let result = match Self::read_and_check(client, &usage_key, &limits_key).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    budget_id,
                    error = %e,
                    "budget_checker: failed to read budget, allowing (advisory)"
                );
                BudgetCheckResult::Ok
            }
        };

        self.cache.insert(budget_id.to_owned(), result);
        &self.cache[budget_id]
    }

    /// Read budget usage and limits, compare each dimension.
    async fn read_and_check(
        client: &ferriskey::Client,
        usage_key: &str,
        limits_key: &str,
    ) -> Result<BudgetCheckResult, ferriskey::Error> {
        // Read all limit dimensions via hgetall (returns HashMap, not flat pairs)
        let limits: std::collections::HashMap<String, String> = client
            .hgetall(limits_key)
            .await
            .unwrap_or_default();

        // Parse hard limits
        for (field, limit_val) in &limits {
            if !field.starts_with("hard:") {
                continue;
            }
            let dimension = &field[5..]; // strip "hard:" prefix
            let limit: u64 = match limit_val.parse() {
                Ok(v) if v > 0 => v,
                _ => continue,
            };

            // Read current usage for this dimension
            let usage_str: Option<String> = client
                .cmd("HGET")
                .arg(usage_key)
                .arg(dimension)
                .execute()
                .await
                .unwrap_or(None);
            let usage: u64 = usage_str
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            if usage >= limit {
                return Ok(BudgetCheckResult::HardBreach {
                    dimension: dimension.to_owned(),
                    detail: format!("budget {}: {} {}/{}", usage_key, dimension, usage, limit),
                });
            }
        }

        Ok(BudgetCheckResult::Ok)
    }

    /// Clear the cache at the start of a new scheduler cycle.
    pub fn reset(&mut self) {
        self.cache.clear();
    }
}

/// Single-lane scheduler with budget/quota pre-checks.
///
/// Iterates execution partitions sequentially, picks the first eligible
/// execution (lowest priority score). Before issuing a claim grant:
/// 1. Check all attached budgets (cross-partition, cached per cycle)
/// 2. Check quota admission (cross-partition FCALL)
/// 3. If any check fails: block the candidate and try next
/// 4. If all pass: issue the claim grant
pub struct Scheduler {
    client: ferriskey::Client,
    config: PartitionConfig,
}

impl Scheduler {
    pub fn new(client: ferriskey::Client, config: PartitionConfig) -> Self {
        Self { client, config }
    }

    /// Find an eligible execution and issue a claim grant.
    ///
    /// Iterates all execution partitions looking for the first partition
    /// with an eligible execution. Issues a claim grant via FCALL.
    ///
    /// Returns `Ok(None)` if no eligible executions exist anywhere.
    /// Returns `Ok(Some(grant))` on success.
    /// Returns `Err` on Valkey errors.
    pub async fn claim_for_worker(
        &self,
        lane: &LaneId,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        grant_ttl_ms: u64,
    ) -> Result<Option<ClaimGrant>, SchedulerError> {
        let num_partitions = self.config.num_execution_partitions;
        let mut budget_checker = BudgetChecker::new(self.config);

        for p_idx in 0..num_partitions {
            let partition = Partition {
                family: PartitionFamily::Execution,
                index: p_idx,
            };
            let idx = IndexKeys::new(&partition);
            let eligible_key = idx.lane_eligible(lane);

            // ZRANGEBYSCORE eligible -inf +inf LIMIT 0 1
            // Lowest score = highest priority candidate
            let candidates: Vec<String> = match self
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
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        error = %e,
                        "scheduler: ZRANGEBYSCORE eligible failed, skipping partition"
                    );
                    continue;
                }
            };

            let eid_str = match candidates.first() {
                Some(s) => s,
                None => continue, // no eligible in this partition
            };

            // Parse the execution ID
            let eid = match ExecutionId::parse(eid_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "scheduler: invalid execution_id in eligible set, skipping"
                    );
                    continue;
                }
            };

            let exec_ctx = ExecKeyContext::new(&partition, &eid);
            let core_key = exec_ctx.core();
            let eid_s = eid.to_string();
            let now_ms = TimestampMs::now().0 as u64;

            // ── Budget pre-check (cross-partition, cached per cycle) ──
            if let Some(block_detail) = self
                .check_budgets(&mut budget_checker, &exec_ctx, &core_key, &eid_s)
                .await?
            {
                // Budget breached — block candidate and try next
                self.block_candidate(
                    &partition, &idx, lane, &eid_s, &eligible_key,
                    "waiting_for_budget", &block_detail, now_ms,
                ).await;
                continue;
            }

            // ── Quota pre-check (cross-partition FCALL on {q:K}) ──
            if let Some(block_detail) = self
                .check_quota(&exec_ctx, &core_key, &eid_s, now_ms)
                .await?
            {
                // Quota denied — block candidate and try next
                self.block_candidate(
                    &partition, &idx, lane, &eid_s, &eligible_key,
                    "waiting_for_quota", &block_detail, now_ms,
                ).await;
                continue;
            }

            // ── All checks passed — issue claim grant ──
            let grant_key = exec_ctx.claim_grant();
            let keys: [&str; 3] = [&core_key, &grant_key, &eligible_key];

            let ttl_str = grant_ttl_ms.to_string();
            let wid_s = worker_id.to_string();
            let wiid_s = worker_instance_id.to_string();
            let lane_s = lane.to_string();

            let argv: [&str; 8] = [
                &eid_s,
                &wid_s,
                &wiid_s,
                &lane_s,
                "",   // capability_hash
                &ttl_str,
                "",   // route_snapshot_json
                "",   // admission_summary
            ];

            match self
                .client
                .fcall::<ferriskey::Value>("ff_issue_claim_grant", &keys, &argv)
                .await
            {
                Ok(_) => {
                    tracing::debug!(
                        partition = p_idx,
                        execution_id = eid_s.as_str(),
                        "scheduler: claim grant issued"
                    );
                    return Ok(Some(ClaimGrant {
                        execution_id: eid,
                        partition,
                        grant_key: grant_key.clone(),
                        expires_at_ms: now_ms + grant_ttl_ms,
                    }));
                }
                Err(e) => {
                    tracing::debug!(
                        partition = p_idx,
                        execution_id = eid_s.as_str(),
                        error = %e,
                        "scheduler: ff_issue_claim_grant rejected, trying next"
                    );
                    continue;
                }
            }
        }

        Ok(None)
    }

    /// Read budget_ids from exec_core and check each. Returns block detail
    /// string if any budget is breached, None if all pass.
    async fn check_budgets(
        &self,
        checker: &mut BudgetChecker,
        _exec_ctx: &ExecKeyContext,
        core_key: &str,
        _eid_s: &str,
    ) -> Result<Option<String>, SchedulerError> {
        // Read budget_ids from exec_core (comma-separated or JSON list)
        let budget_ids_str: Option<String> = self
            .client
            .cmd("HGET")
            .arg(core_key)
            .arg("budget_ids")
            .execute()
            .await?;

        let budget_ids_str = match budget_ids_str {
            Some(s) => s,
            None => return Ok(None),
        };
        if budget_ids_str.is_empty() {
            return Ok(None); // no budgets attached
        }

        // Parse comma-separated budget IDs
        for budget_id in budget_ids_str.split(',') {
            let budget_id = budget_id.trim();
            if budget_id.is_empty() {
                continue;
            }
            let result = checker.check_budget(&self.client, budget_id).await;
            if let BudgetCheckResult::HardBreach { detail, .. } = result {
                return Ok(Some(detail.clone()));
            }
        }

        Ok(None)
    }

    /// Check quota admission for the candidate. Returns block detail if denied.
    async fn check_quota(
        &self,
        _exec_ctx: &ExecKeyContext,
        core_key: &str,
        eid_s: &str,
        now_ms: u64,
    ) -> Result<Option<String>, SchedulerError> {
        // Read quota_policy_id from exec_core
        let quota_id_str: Option<String> = self
            .client
            .cmd("HGET")
            .arg(core_key)
            .arg("quota_policy_id")
            .execute()
            .await?;

        let quota_id_str = match quota_id_str {
            Some(s) => s,
            None => return Ok(None),
        };
        if quota_id_str.is_empty() {
            return Ok(None); // no quota attached
        }

        // Compute real {q:K} partition tag from quota_policy_id
        let tag = match QuotaPolicyId::parse(&quota_id_str) {
            Ok(qid) => {
                let partition = quota_partition(&qid, &self.config);
                partition.hash_tag()
            }
            Err(_) => "{q:0}".to_owned(), // fallback for non-UUID test IDs
        };

        let quota_def_key = format!("ff:quota:{}:{}", tag, quota_id_str);
        let window_key = format!("ff:quota:{}:{}:window:requests_per_window", tag, quota_id_str);
        let concurrency_key = format!("ff:quota:{}:{}:concurrency", tag, quota_id_str);
        let admitted_key = format!("ff:quota:{}:{}:admitted:{}", tag, quota_id_str, eid_s);
        let admitted_set_key = format!("ff:quota:{}:{}:admitted_set", tag, quota_id_str);

        // Read quota limits from policy hash
        let rate_limit: Option<String> = self.client
            .cmd("HGET").arg(&quota_def_key).arg("requests_per_window_limit")
            .execute().await.unwrap_or(None);
        let window_secs: Option<String> = self.client
            .cmd("HGET").arg(&quota_def_key).arg("requests_per_window_seconds")
            .execute().await.unwrap_or(None);
        let concurrency_cap: Option<String> = self.client
            .cmd("HGET").arg(&quota_def_key).arg("active_concurrency_cap")
            .execute().await.unwrap_or(None);
        let jitter: Option<String> = self.client
            .cmd("HGET").arg(&quota_def_key).arg("jitter_ms")
            .execute().await.unwrap_or(None);

        let rate_limit = rate_limit.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);
        let window_secs = window_secs.as_deref().and_then(|s| s.parse().ok()).unwrap_or(60u64);
        let concurrency_cap = concurrency_cap.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);
        let jitter_ms = jitter.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);

        // No limits configured — admit
        if rate_limit == 0 && concurrency_cap == 0 {
            return Ok(None);
        }

        // FCALL ff_check_admission_and_record on {q:K}
        let keys: [&str; 5] = [&window_key, &concurrency_key, &quota_def_key, &admitted_key, &admitted_set_key];
        let now_s = now_ms.to_string();
        let ws = window_secs.to_string();
        let rl = rate_limit.to_string();
        let cc = concurrency_cap.to_string();
        let jt = jitter_ms.to_string();
        let argv: [&str; 6] = [&now_s, &ws, &rl, &cc, eid_s, &jt];

        match self.client
            .fcall::<ferriskey::Value>("ff_check_admission_and_record", &keys, &argv)
            .await
        {
            Ok(result) => {
                // Parse domain-specific result: {"ADMITTED"}, {"RATE_EXCEEDED", retry_after},
                // {"CONCURRENCY_EXCEEDED"}, {"ALREADY_ADMITTED"}
                let status = Self::parse_admission_status(&result);
                match status.as_str() {
                    "ADMITTED" | "ALREADY_ADMITTED" => Ok(None),
                    "RATE_EXCEEDED" => Ok(Some(format!(
                        "quota {}: rate limit {}/{} per {}s window",
                        quota_id_str, rate_limit, rate_limit, window_secs
                    ))),
                    "CONCURRENCY_EXCEEDED" => Ok(Some(format!(
                        "quota {}: concurrency cap {}",
                        quota_id_str, concurrency_cap
                    ))),
                    _ => {
                        tracing::warn!(
                            quota_id = quota_id_str.as_str(),
                            status = status.as_str(),
                            "scheduler: unexpected admission result"
                        );
                        Ok(None) // allow on unknown status (advisory)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    quota_id = quota_id_str.as_str(),
                    error = %e,
                    "scheduler: quota FCALL failed, allowing (advisory)"
                );
                Ok(None) // allow on FCALL error (advisory — budget HGET errors propagate above)
            }
        }
    }

    /// Parse the first element of a Valkey array result as a status string.
    fn parse_admission_status(result: &ferriskey::Value) -> String {
        match result {
            ferriskey::Value::Array(arr) => {
                match arr.first() {
                    Some(Ok(ferriskey::Value::BulkString(b))) => {
                        String::from_utf8_lossy(b).into_owned()
                    }
                    Some(Ok(ferriskey::Value::SimpleString(s))) => s.clone(),
                    _ => "UNKNOWN".to_owned(),
                }
            }
            _ => "UNKNOWN".to_owned(),
        }
    }

    /// Block a candidate that failed budget/quota check.
    /// FCALL ff_block_execution_for_admission on {p:N}.
    #[allow(clippy::too_many_arguments)]
    async fn block_candidate(
        &self,
        partition: &Partition,
        idx: &IndexKeys,
        lane: &LaneId,
        eid_s: &str,
        eligible_key: &str,
        block_reason: &str,
        blocking_detail: &str,
        now_ms: u64,
    ) {
        let exec_ctx = ExecKeyContext::new(partition, &ExecutionId::parse(eid_s).unwrap());
        let core_key = exec_ctx.core();
        let blocked_key = match block_reason {
            "waiting_for_budget" => idx.lane_blocked_budget(lane),
            "waiting_for_quota" => idx.lane_blocked_quota(lane),
            _ => idx.lane_blocked_budget(lane),
        };

        let keys: [&str; 3] = [&core_key, eligible_key, &blocked_key];
        let now_s = now_ms.to_string();
        let argv: [&str; 4] = [eid_s, block_reason, blocking_detail, &now_s];

        match self.client
            .fcall::<ferriskey::Value>("ff_block_execution_for_admission", &keys, &argv)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    execution_id = eid_s,
                    reason = block_reason,
                    "scheduler: candidate blocked by admission check"
                );
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_s,
                    error = %e,
                    "scheduler: ff_block_execution_for_admission failed"
                );
            }
        }
    }
}

/// Errors from the scheduler.
#[derive(Debug)]
pub enum SchedulerError {
    Valkey(ferriskey::Error),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valkey(e) => write!(f, "valkey error: {e}"),
        }
    }
}

impl std::error::Error for SchedulerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Valkey(e) => Some(e),
        }
    }
}

impl From<ferriskey::Error> for SchedulerError {
    fn from(e: ferriskey::Error) -> Self {
        Self::Valkey(e)
    }
}


