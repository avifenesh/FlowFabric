//! Assertion macros and helpers for FlowFabric integration tests.
//!
//! These macros read Valkey state via ferriskey and compare against expected values,
//! producing rich error messages on failure.

use std::collections::HashMap;

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::state::*;
use ff_core::types::*;

use crate::fixtures::TestCluster;

// ─── State Vector Assertion ───

/// Expected state vector for assertion. All fields are optional — only provided
/// fields are checked, allowing partial assertions.
#[derive(Clone, Debug, Default)]
pub struct ExpectedState {
    pub lifecycle_phase: Option<LifecyclePhase>,
    pub ownership_state: Option<OwnershipState>,
    pub eligibility_state: Option<EligibilityState>,
    pub blocking_reason: Option<BlockingReason>,
    pub terminal_outcome: Option<TerminalOutcome>,
    pub attempt_state: Option<AttemptState>,
    pub public_state: Option<PublicState>,
}

/// Read the execution core hash and verify state vector dimensions.
///
/// Panics with a descriptive message on mismatch.
pub async fn assert_execution_state(
    tc: &TestCluster,
    eid: &ExecutionId,
    expected: &ExpectedState,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let fields = tc.read_exec_core(&core_key).await;
    assert!(
        !fields.is_empty(),
        "execution core hash not found: {core_key}"
    );

    if let Some(expected_val) = &expected.lifecycle_phase {
        assert_field_eq(&fields, "lifecycle_phase", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.ownership_state {
        assert_field_eq(&fields, "ownership_state", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.eligibility_state {
        assert_field_eq(&fields, "eligibility_state", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.blocking_reason {
        assert_field_eq(&fields, "blocking_reason", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.terminal_outcome {
        assert_field_eq(&fields, "terminal_outcome", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.attempt_state {
        assert_field_eq(&fields, "attempt_state", expected_val, &core_key);
    }
    if let Some(expected_val) = &expected.public_state {
        assert_field_eq(&fields, "public_state", expected_val, &core_key);
    }
}

/// Verify that all 7 state vector dimensions are set (non-empty) on the core hash.
/// This catches bugs where a Lua script forgets to set one or more dimensions.
pub async fn assert_state_vector_complete(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let fields = tc.read_exec_core(&core_key).await;
    assert!(
        !fields.is_empty(),
        "execution core hash not found: {core_key}"
    );

    let required = [
        "lifecycle_phase",
        "ownership_state",
        "eligibility_state",
        "blocking_reason",
        "terminal_outcome",
        "attempt_state",
        "public_state",
    ];

    for dim in required {
        let value = fields.get(dim);
        assert!(
            value.is_some() && !value.unwrap().is_empty(),
            "state vector dimension '{dim}' missing or empty on {core_key}. \
             All 7 dimensions must be set on every HSET. \
             Fields present: {fields:?}"
        );
    }
}

/// Verify that stored public_state matches the derivation from the other 6 dimensions.
pub async fn assert_public_state_consistent(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let fields = tc.read_exec_core(&core_key).await;
    assert!(
        !fields.is_empty(),
        "execution core hash not found: {core_key}"
    );

    let sv = parse_state_vector(&fields)
        .unwrap_or_else(|e| panic!("failed to parse state vector from {core_key}: {e}"));

    let derived = sv.derive_public_state();
    assert_eq!(
        sv.public_state, derived,
        "public_state inconsistency on {core_key}: \
         stored={:?}, derived={:?}. State vector: {:?}",
        sv.public_state, derived, sv
    );
}

// ─── Index Membership Assertions ───

/// Assert that an execution IS a member of the given ZSET index.
pub async fn assert_in_index(
    tc: &TestCluster,
    index_key: &str,
    eid: &ExecutionId,
) {
    let score = tc.zscore(index_key, &eid.to_string()).await;
    assert!(
        score.is_some(),
        "expected {eid} to be in index {index_key}, but ZSCORE returned nil"
    );
}

/// Assert that an execution is NOT a member of the given ZSET index.
pub async fn assert_not_in_index(
    tc: &TestCluster,
    index_key: &str,
    eid: &ExecutionId,
) {
    let score = tc.zscore(index_key, &eid.to_string()).await;
    assert!(
        score.is_none(),
        "expected {eid} to NOT be in index {index_key}, but ZSCORE returned {score:?}"
    );
}

/// Assert that an execution IS a member of the given SET index.
pub async fn assert_in_set(
    tc: &TestCluster,
    set_key: &str,
    eid: &ExecutionId,
) {
    let is_member = tc.sismember(set_key, &eid.to_string()).await;
    assert!(
        is_member,
        "expected {eid} to be in set {set_key}, but SISMEMBER returned 0"
    );
}

/// Assert that an execution is NOT a member of the given SET index.
pub async fn assert_not_in_set(
    tc: &TestCluster,
    set_key: &str,
    eid: &ExecutionId,
) {
    let is_member = tc.sismember(set_key, &eid.to_string()).await;
    assert!(
        !is_member,
        "expected {eid} to NOT be in set {set_key}, but SISMEMBER returned 1"
    );
}

/// Convenience: assert execution is in the eligible ZSET for its lane.
pub async fn assert_in_eligible(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane_id: &LaneId,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let idx = IndexKeys::new(&partition);
    let key = idx.lane_eligible(lane_id);
    assert_in_index(tc, &key, eid).await;
}

/// Convenience: assert execution is NOT in the eligible ZSET.
pub async fn assert_not_in_eligible(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane_id: &LaneId,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let idx = IndexKeys::new(&partition);
    let key = idx.lane_eligible(lane_id);
    assert_not_in_index(tc, &key, eid).await;
}

/// Convenience: assert execution is in the terminal ZSET for its lane.
pub async fn assert_in_terminal(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane_id: &LaneId,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let idx = IndexKeys::new(&partition);
    let key = idx.lane_terminal(lane_id);
    assert_in_index(tc, &key, eid).await;
}

/// Convenience: assert execution is in the active ZSET for its lane.
pub async fn assert_in_active(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane_id: &LaneId,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let idx = IndexKeys::new(&partition);
    let key = idx.lane_active(lane_id);
    assert_in_index(tc, &key, eid).await;
}

/// Convenience: assert execution is in the all_executions SET.
pub async fn assert_in_all_executions(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let idx = IndexKeys::new(&partition);
    let key = idx.all_executions();
    assert_in_set(tc, &key, eid).await;
}

// ─── Attempt State Assertions ───

/// Assert the state of an attempt record.
pub async fn assert_attempt_state(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: AttemptIndex,
    expected_state: &str,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let attempt_key = ctx.attempt_hash(attempt_index);

    let state = tc.hget(&attempt_key, "attempt_state").await;
    assert_eq!(
        state.as_deref(),
        Some(expected_state),
        "attempt {attempt_index} on {eid}: expected state '{expected_state}', got {state:?}. \
         Key: {attempt_key}"
    );
}

/// Assert that an attempt record exists.
pub async fn assert_attempt_exists(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: AttemptIndex,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let attempt_key = ctx.attempt_hash(attempt_index);
    assert!(
        tc.exists(&attempt_key).await,
        "expected attempt {attempt_index} to exist for {eid}, but key {attempt_key} not found"
    );
}

/// Assert the attempt_type field on an attempt record.
pub async fn assert_attempt_type(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: AttemptIndex,
    expected_type: &str,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let attempt_key = ctx.attempt_hash(attempt_index);

    let attempt_type = tc.hget(&attempt_key, "attempt_type").await;
    assert_eq!(
        attempt_type.as_deref(),
        Some(expected_type),
        "attempt {attempt_index} on {eid}: expected type '{expected_type}', got {attempt_type:?}"
    );
}

// ─── Lease Assertions ───

/// Assert that a lease record exists for the execution.
pub async fn assert_lease_active(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let lease_key = ctx.lease_current();
    assert!(
        tc.exists(&lease_key).await,
        "expected active lease for {eid}, but key {lease_key} not found"
    );
}

/// Assert that no lease record exists for the execution.
pub async fn assert_lease_absent(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let lease_key = ctx.lease_current();
    assert!(
        !tc.exists(&lease_key).await,
        "expected no lease for {eid}, but key {lease_key} exists"
    );
}

/// Assert that the lease epoch on exec_core matches the expected value.
pub async fn assert_lease_epoch(
    tc: &TestCluster,
    eid: &ExecutionId,
    expected_epoch: u64,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let epoch_str = tc.hget(&core_key, "current_lease_epoch").await;
    let epoch: u64 = epoch_str
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    assert_eq!(
        epoch, expected_epoch,
        "lease epoch mismatch on {eid}: expected {expected_epoch}, got {epoch}"
    );
}

// ─── Stream Assertions ───

/// Assert that a stream exists for the given attempt.
pub async fn assert_stream_exists(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: AttemptIndex,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let stream_key = ctx.stream(attempt_index);
    assert!(
        tc.exists(&stream_key).await,
        "expected stream for {eid} attempt {attempt_index}, but key {stream_key} not found"
    );
}

// ─── Blocking Detail Assertion ───

/// Assert blocking_detail field on exec_core.
pub async fn assert_blocking_detail(
    tc: &TestCluster,
    eid: &ExecutionId,
    expected_detail: &str,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let core_key = ctx.core();

    let detail = tc.hget(&core_key, "blocking_detail").await;
    assert_eq!(
        detail.as_deref(),
        Some(expected_detail),
        "blocking_detail mismatch on {eid}: expected '{expected_detail}', got {detail:?}"
    );
}

// ─── Internal helpers ───

/// Compare a serde-serializable expected value against a hash field string.
fn assert_field_eq<T: serde::Serialize>(
    fields: &HashMap<String, String>,
    field_name: &str,
    expected: &T,
    context_key: &str,
) {
    let actual = fields.get(field_name);
    // Serialize expected to the serde snake_case string for comparison
    let expected_str = serde_json::to_value(expected)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_owned()));

    match (actual, &expected_str) {
        (Some(actual_val), Some(expected_val)) => {
            assert_eq!(
                actual_val, expected_val,
                "field '{field_name}' mismatch on {context_key}: \
                 expected '{expected_val}', got '{actual_val}'. \
                 All fields: {fields:?}"
            );
        }
        (None, _) => {
            panic!(
                "field '{field_name}' not found on {context_key}. \
                 Expected '{expected_str:?}'. Fields present: {fields:?}"
            );
        }
        (Some(actual_val), None) => {
            panic!(
                "could not serialize expected value for '{field_name}'. \
                 Actual value: '{actual_val}'"
            );
        }
    }
}

/// Parse a state vector from hash field strings.
fn parse_state_vector(fields: &HashMap<String, String>) -> Result<StateVector, String> {
    let get = |name: &str| -> Result<String, String> {
        fields
            .get(name)
            .cloned()
            .ok_or_else(|| format!("missing field: {name}"))
    };

    let parse_json = |name: &str, val: &str| -> Result<serde_json::Value, String> {
        serde_json::from_str(&format!("\"{val}\""))
            .map_err(|e| format!("failed to parse {name}='{val}': {e}"))
    };

    let deser = |name: &str| -> Result<serde_json::Value, String> {
        let val = get(name)?;
        parse_json(name, &val)
    };

    Ok(StateVector {
        lifecycle_phase: serde_json::from_value(deser("lifecycle_phase")?)
            .map_err(|e| format!("lifecycle_phase: {e}"))?,
        ownership_state: serde_json::from_value(deser("ownership_state")?)
            .map_err(|e| format!("ownership_state: {e}"))?,
        eligibility_state: serde_json::from_value(deser("eligibility_state")?)
            .map_err(|e| format!("eligibility_state: {e}"))?,
        blocking_reason: serde_json::from_value(deser("blocking_reason")?)
            .map_err(|e| format!("blocking_reason: {e}"))?,
        terminal_outcome: serde_json::from_value(deser("terminal_outcome")?)
            .map_err(|e| format!("terminal_outcome: {e}"))?,
        attempt_state: serde_json::from_value(deser("attempt_state")?)
            .map_err(|e| format!("attempt_state: {e}"))?,
        public_state: serde_json::from_value(deser("public_state")?)
            .map_err(|e| format!("public_state: {e}"))?,
    })
}

// ─── Convenience macros ───

/// Assert execution state with named fields.
///
/// ```rust,ignore
/// assert_execution_state!(tc, eid,
///     lifecycle_phase = LifecyclePhase::Terminal,
///     terminal_outcome = TerminalOutcome::Success,
///     public_state = PublicState::Completed,
/// );
/// ```
#[macro_export]
macro_rules! assert_execution_state {
    ($tc:expr, $eid:expr, $($field:ident = $value:expr),+ $(,)?) => {{
        let expected = $crate::assertions::ExpectedState {
            $($field: Some($value),)*
            ..Default::default()
        };
        $crate::assertions::assert_execution_state($tc, $eid, &expected).await;
    }};
}

/// Assert index membership (ZSET).
///
/// ```rust,ignore
/// assert_index_membership!(tc, &index_key, eid, true);  // should be member
/// assert_index_membership!(tc, &index_key, eid, false); // should NOT be member
/// ```
#[macro_export]
macro_rules! assert_index_membership {
    ($tc:expr, $key:expr, $eid:expr, true) => {
        $crate::assertions::assert_in_index($tc, $key, $eid).await;
    };
    ($tc:expr, $key:expr, $eid:expr, false) => {
        $crate::assertions::assert_not_in_index($tc, $key, $eid).await;
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_state_default_is_all_none() {
        let es = ExpectedState::default();
        assert!(es.lifecycle_phase.is_none());
        assert!(es.ownership_state.is_none());
        assert!(es.public_state.is_none());
    }

    #[test]
    fn parse_state_vector_from_hash_fields() {
        let mut fields = HashMap::new();
        fields.insert("lifecycle_phase".into(), "active".into());
        fields.insert("ownership_state".into(), "leased".into());
        fields.insert("eligibility_state".into(), "not_applicable".into());
        fields.insert("blocking_reason".into(), "none".into());
        fields.insert("terminal_outcome".into(), "none".into());
        fields.insert("attempt_state".into(), "running_attempt".into());
        fields.insert("public_state".into(), "active".into());

        let sv = parse_state_vector(&fields).unwrap();
        assert_eq!(sv.lifecycle_phase, LifecyclePhase::Active);
        assert_eq!(sv.ownership_state, OwnershipState::Leased);
        assert_eq!(sv.public_state, PublicState::Active);
        assert!(sv.is_consistent());
    }

    #[test]
    fn parse_state_vector_missing_field() {
        let mut fields = HashMap::new();
        fields.insert("lifecycle_phase".into(), "active".into());
        // Missing other fields
        let result = parse_state_vector(&fields);
        assert!(result.is_err());
    }
}
