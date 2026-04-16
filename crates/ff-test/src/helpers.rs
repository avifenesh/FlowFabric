//! Test helper functions for FlowFabric integration tests.
//!
//! Provides shorthand builders for common test scenarios. These helpers
//! use raw FCALL calls where ff-script typed wrappers are not yet available,
//! and will be updated to use typed wrappers once Step 1.2 completes.

use std::collections::HashMap;
use std::time::Duration;

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::policy::ExecutionPolicy;
use ff_core::state::*;
use ff_core::types::*;

use crate::fixtures::TestCluster;

/// Parameters for creating a test execution.
#[derive(Clone, Debug)]
pub struct CreateTestExecutionParams {
    pub execution_id: ExecutionId,
    pub namespace: Namespace,
    pub lane_id: LaneId,
    pub execution_kind: String,
    pub input_payload: String,
    pub priority: i32,
    pub creator_identity: String,
    pub tags: HashMap<String, String>,
    pub policy: ExecutionPolicy,
    pub delay_until: Option<TimestampMs>,
}

impl CreateTestExecutionParams {
    /// Create default params with a fresh execution ID.
    pub fn new(tc: &TestCluster) -> Self {
        Self {
            execution_id: tc.new_execution_id(),
            namespace: tc.test_namespace(),
            lane_id: tc.test_lane(),
            execution_kind: "test_execution".to_owned(),
            input_payload: r#"{"test": true}"#.to_owned(),
            priority: 0,
            creator_identity: "ff-test".to_owned(),
            tags: HashMap::new(),
            policy: ExecutionPolicy::default(),
            delay_until: None,
        }
    }

    /// Set priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Set execution kind.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.execution_kind = kind.into();
        self
    }

    /// Set delay.
    pub fn with_delay(mut self, delay_until: TimestampMs) -> Self {
        self.delay_until = Some(delay_until);
        self
    }

    /// Add a tag.
    pub fn with_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.insert(key.into(), value.into());
        self
    }

    /// Set the policy.
    pub fn with_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the lane.
    pub fn with_lane(mut self, lane: LaneId) -> Self {
        self.lane_id = lane;
        self
    }

    /// Set the namespace.
    pub fn with_namespace(mut self, ns: Namespace) -> Self {
        self.namespace = ns;
        self
    }
}

/// Create a test execution by directly writing to Valkey hashes.
///
/// This is a **test-only bypass** that writes execution state directly,
/// without going through FCALL. Useful for setting up test preconditions
/// before the Lua functions exist.
///
/// Once ff-script provides `ff_create_execution`, tests should prefer
/// [`create_test_execution_via_fcall`] for realistic testing.
pub async fn create_test_execution_direct(
    tc: &TestCluster,
    params: &CreateTestExecutionParams,
) -> ExecutionId {
    let config = tc.partition_config();
    let partition = execution_partition(&params.execution_id, config);
    let ctx = ExecKeyContext::new(&partition, &params.execution_id);
    let idx = IndexKeys::new(&partition);
    let now = tc.now();

    // Determine initial state
    let (eligibility, blocking, public) = if params.delay_until.is_some() {
        (
            "not_eligible_until_time",
            "waiting_for_delay",
            "delayed",
        )
    } else {
        ("eligible_now", "waiting_for_worker", "waiting")
    };

    // Write exec_core hash — all 7 state vector dimensions
    let core_key = ctx.core();
    let fields: Vec<(&str, String)> = vec![
        ("execution_id", params.execution_id.to_string()),
        ("namespace", params.namespace.to_string()),
        ("lane_id", params.lane_id.to_string()),
        ("execution_kind", params.execution_kind.clone()),
        ("partition_id", partition.index.to_string()),
        ("priority", params.priority.to_string()),
        ("creator_identity", params.creator_identity.clone()),
        ("lifecycle_phase", "runnable".to_owned()),
        ("ownership_state", "unowned".to_owned()),
        ("eligibility_state", eligibility.to_owned()),
        ("blocking_reason", blocking.to_owned()),
        ("blocking_detail", String::new()),
        ("terminal_outcome", "none".to_owned()),
        ("attempt_state", "pending_first_attempt".to_owned()),
        ("public_state", public.to_owned()),
        ("current_attempt_index", "0".to_owned()),
        ("total_attempt_count", "0".to_owned()),
        ("created_at", now.to_string()),
        ("last_transition_at", now.to_string()),
        ("last_mutation_at", now.to_string()),
        ("retry_count", "0".to_owned()),
        ("reclaim_count", "0".to_owned()),
        ("replay_count", "0".to_owned()),
        ("fallback_index", "0".to_owned()),
    ];

    // HSET all fields
    for (field, value) in &fields {
        tc.client()
            .hset(&core_key, *field, value.as_str())
            .await
            .unwrap_or_else(|e| panic!("HSET {core_key} {field} failed: {e}"));
    }

    // Write payload
    let payload_key = ctx.payload();
    tc.client()
        .set(&payload_key, &params.input_payload)
        .await
        .unwrap_or_else(|e| panic!("SET {payload_key} failed: {e}"));

    // Write policy
    let policy_key = ctx.policy();
    let policy_json = serde_json::to_string(&params.policy).unwrap();
    tc.client()
        .set(&policy_key, &policy_json)
        .await
        .unwrap_or_else(|e| panic!("SET {policy_key} failed: {e}"));

    // Write tags
    if !params.tags.is_empty() {
        let tags_key = ctx.tags();
        for (k, v) in &params.tags {
            tc.client()
                .hset(&tags_key, k.as_str(), v.as_str())
                .await
                .unwrap_or_else(|e| panic!("HSET {tags_key} {k} failed: {e}"));
        }
    }

    // SADD to all_executions
    let all_exec_key = idx.all_executions();
    let eid_str = params.execution_id.to_string();
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(&all_exec_key)
        .arg(&eid_str)
        .execute()
        .await
        .unwrap_or_else(|e| panic!("SADD {all_exec_key} failed: {e}"));

    // ZADD to eligible or delayed index
    if let Some(delay_until) = params.delay_until {
        let delayed_key = idx.lane_delayed(&params.lane_id);
        let _: i64 = tc
            .client()
            .cmd("ZADD")
            .arg(&delayed_key)
            .arg(delay_until.0.to_string())
            .arg(&eid_str)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("ZADD {delayed_key} failed: {e}"));
    } else {
        // Priority score: -(priority * 1_000_000_000_000) + created_at_ms
        let score = -(params.priority as i64) * 1_000_000_000_000 + now.0;
        let eligible_key = idx.lane_eligible(&params.lane_id);
        let _: i64 = tc
            .client()
            .cmd("ZADD")
            .arg(&eligible_key)
            .arg(score.to_string())
            .arg(&eid_str)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("ZADD {eligible_key} failed: {e}"));
    }

    params.execution_id.clone()
}


/// Poll until the execution reaches the expected public state, or timeout.
///
/// Polls every 50ms. Panics if the timeout is reached without matching.
pub async fn wait_for_state(
    tc: &TestCluster,
    eid: &ExecutionId,
    expected: PublicState,
    timeout: Duration,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let expected_str = serde_json::to_value(expected)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .unwrap_or_else(|| format!("{expected:?}"));

    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        let current = tc.hget(&core_key, "public_state").await;

        if current.as_deref() == Some(&expected_str) {
            return;
        }

        if start.elapsed() >= timeout {
            panic!(
                "timeout waiting for {eid} to reach public_state={expected_str}. \
                 Current state: {current:?}. Waited {:?}.",
                timeout
            );
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Poll until the execution has a specific field value, or timeout.
pub async fn wait_for_field(
    tc: &TestCluster,
    eid: &ExecutionId,
    field: &str,
    expected_value: &str,
    timeout: Duration,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        let current = tc.hget(&core_key, field).await;

        if current.as_deref() == Some(expected_value) {
            return;
        }

        if start.elapsed() >= timeout {
            panic!(
                "timeout waiting for {eid} field '{field}' = '{expected_value}'. \
                 Current: {current:?}. Waited {:?}.",
                timeout
            );
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Read the current public state of an execution (or None if not found).
pub async fn get_public_state(tc: &TestCluster, eid: &ExecutionId) -> Option<String> {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();
    tc.hget(&core_key, "public_state").await
}

/// Read the full exec_core as a HashMap.
pub async fn get_exec_core(
    tc: &TestCluster,
    eid: &ExecutionId,
) -> HashMap<String, String> {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.read_exec_core(&ctx.core()).await
}

/// Count members in a ZSET.
pub async fn zcard(tc: &TestCluster, key: &str) -> i64 {
    tc.client()
        .cmd("ZCARD")
        .arg(key)
        .execute()
        .await
        .unwrap_or_else(|e| panic!("ZCARD {key} failed: {e}"))
}

/// Count members in a SET.
pub async fn scard(tc: &TestCluster, key: &str) -> i64 {
    tc.client()
        .cmd("SCARD")
        .arg(key)
        .execute()
        .await
        .unwrap_or_else(|e| panic!("SCARD {key} failed: {e}"))
}
