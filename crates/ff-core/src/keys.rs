use crate::partition::Partition;
use crate::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, QuotaPolicyId, SignalId,
    WaitpointId, WaitpointKey, WorkerInstanceId,
};

// ─── Execution Partition Keys ({p:N}) ───

/// Pre-computed key context for all keys on an execution's partition.
/// Created once per operation and reused for all key construction.
pub struct ExecKeyContext {
    /// The hash tag, e.g. `{p:42}`
    tag: String,
    /// The execution ID string
    eid: String,
}

impl ExecKeyContext {
    pub fn new(partition: &Partition, eid: &ExecutionId) -> Self {
        Self {
            tag: partition.hash_tag(),
            eid: eid.to_string(),
        }
    }

    // ── Execution Core (RFC-001) ──

    /// `ff:exec:{p:N}:<eid>:core` — Authoritative execution record.
    pub fn core(&self) -> String {
        format!("ff:exec:{}:{}:core", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:payload` — Opaque input payload.
    pub fn payload(&self) -> String {
        format!("ff:exec:{}:{}:payload", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:result` — Opaque result payload.
    pub fn result(&self) -> String {
        format!("ff:exec:{}:{}:result", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:policy` — JSON-encoded ExecutionPolicySnapshot.
    pub fn policy(&self) -> String {
        format!("ff:exec:{}:{}:policy", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:tags` — User-supplied key-value labels.
    pub fn tags(&self) -> String {
        format!("ff:exec:{}:{}:tags", self.tag, self.eid)
    }

    // ── Lease (RFC-003) ──

    /// `ff:exec:{p:N}:<eid>:lease:current` — Full lease object.
    pub fn lease_current(&self) -> String {
        format!("ff:exec:{}:{}:lease:current", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:lease:history` — Append-only lease events.
    pub fn lease_history(&self) -> String {
        format!("ff:exec:{}:{}:lease:history", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:claim_grant` — Ephemeral claim grant.
    pub fn claim_grant(&self) -> String {
        format!("ff:exec:{}:{}:claim_grant", self.tag, self.eid)
    }

    // ── Attempt (RFC-002) ──

    /// `ff:exec:{p:N}:<eid>:attempts` — Attempt index ZSET.
    pub fn attempts(&self) -> String {
        format!("ff:exec:{}:{}:attempts", self.tag, self.eid)
    }

    /// `ff:attempt:{p:N}:<eid>:<attempt_index>` — Per-attempt detail.
    pub fn attempt_hash(&self, index: AttemptIndex) -> String {
        format!("ff:attempt:{}:{}:{}", self.tag, self.eid, index)
    }

    /// `ff:attempt:{p:N}:<eid>:<attempt_index>:usage` — Per-attempt usage counters.
    pub fn attempt_usage(&self, index: AttemptIndex) -> String {
        format!("ff:attempt:{}:{}:{}:usage", self.tag, self.eid, index)
    }

    /// `ff:attempt:{p:N}:<eid>:<attempt_index>:policy` — Frozen attempt policy snapshot.
    pub fn attempt_policy(&self, index: AttemptIndex) -> String {
        format!("ff:attempt:{}:{}:{}:policy", self.tag, self.eid, index)
    }

    // ── Stream (RFC-006) ──

    /// `ff:stream:{p:N}:<eid>:<attempt_index>` — Attempt-scoped output stream.
    pub fn stream(&self, index: AttemptIndex) -> String {
        format!("ff:stream:{}:{}:{}", self.tag, self.eid, index)
    }

    /// `ff:stream:{p:N}:<eid>:<attempt_index>:meta` — Stream metadata.
    pub fn stream_meta(&self, index: AttemptIndex) -> String {
        format!("ff:stream:{}:{}:{}:meta", self.tag, self.eid, index)
    }

    // ── Suspension / Waitpoint (RFC-004) ──

    /// `ff:exec:{p:N}:<eid>:suspension:current` — Current suspension episode.
    pub fn suspension_current(&self) -> String {
        format!("ff:exec:{}:{}:suspension:current", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:waitpoints` — Set of all waitpoint IDs.
    pub fn waitpoints(&self) -> String {
        format!("ff:exec:{}:{}:waitpoints", self.tag, self.eid)
    }

    /// `ff:wp:{p:N}:<wp_id>` — Waitpoint record.
    pub fn waitpoint(&self, wp_id: &WaitpointId) -> String {
        format!("ff:wp:{}:{}", self.tag, wp_id)
    }

    /// `ff:wp:{p:N}:<wp_id>:signals` — Per-waitpoint signal history.
    pub fn waitpoint_signals(&self, wp_id: &WaitpointId) -> String {
        format!("ff:wp:{}:{}:signals", self.tag, wp_id)
    }

    /// `ff:wp:{p:N}:<wp_id>:condition` — Resume condition evaluation state.
    pub fn waitpoint_condition(&self, wp_id: &WaitpointId) -> String {
        format!("ff:wp:{}:{}:condition", self.tag, wp_id)
    }

    // ── Signal (RFC-005) ──

    /// `ff:signal:{p:N}:<signal_id>` — Signal record.
    pub fn signal(&self, sig_id: &SignalId) -> String {
        format!("ff:signal:{}:{}", self.tag, sig_id)
    }

    /// `ff:signal:{p:N}:<signal_id>:payload` — Opaque signal payload.
    pub fn signal_payload(&self, sig_id: &SignalId) -> String {
        format!("ff:signal:{}:{}:payload", self.tag, sig_id)
    }

    /// `ff:exec:{p:N}:<eid>:signals` — Per-execution signal index.
    pub fn exec_signals(&self) -> String {
        format!("ff:exec:{}:{}:signals", self.tag, self.eid)
    }

    /// `ff:sigdedup:{p:N}:<wp_id>:<idem_key>` — Signal idempotency guard.
    pub fn signal_dedup(&self, wp_id: &WaitpointId, idempotency_key: &str) -> String {
        format!("ff:sigdedup:{}:{}:{}", self.tag, wp_id, idempotency_key)
    }

    // ── Flow Dependency — Execution-Local (RFC-007) ──

    /// `ff:exec:{p:N}:<eid>:deps:meta` — Dependency summary.
    pub fn deps_meta(&self) -> String {
        format!("ff:exec:{}:{}:deps:meta", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:dep:<edge_id>` — Per-edge local state.
    pub fn dep_edge(&self, edge_id: &EdgeId) -> String {
        format!("ff:exec:{}:{}:dep:{}", self.tag, self.eid, edge_id)
    }

    /// `ff:exec:{p:N}:<eid>:deps:unresolved` — Set of unresolved edge IDs.
    pub fn deps_unresolved(&self) -> String {
        format!("ff:exec:{}:{}:deps:unresolved", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:deps:all_edges` — Set of ALL applied edge IDs
    /// on this execution (unresolved + satisfied + impossible). Populated by
    /// ff_apply_dependency_to_child, never pruned on resolve. Used by the
    /// retention trimmer to enumerate dep edge hashes without SCAN.
    pub fn deps_all_edges(&self) -> String {
        format!("ff:exec:{}:{}:deps:all_edges", self.tag, self.eid)
    }

    // ── Accessor ──

    /// Dummy key on this partition, used as a placeholder for unused KEYS
    /// positions (e.g. empty idempotency key). Ensures all KEYS in an FCALL
    /// share the same hash tag, preventing CrossSlot errors in cluster mode.
    pub fn noop(&self) -> String {
        format!("ff:noop:{}", self.tag)
    }

    pub fn hash_tag(&self) -> &str {
        &self.tag
    }

    pub fn execution_id_str(&self) -> &str {
        &self.eid
    }
}

// ─── Partition-Local Index Keys ({p:N}) ───

/// Index keys scoped to a single execution partition.
pub struct IndexKeys {
    tag: String,
}

impl IndexKeys {
    pub fn new(partition: &Partition) -> Self {
        Self {
            tag: partition.hash_tag(),
        }
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:eligible`
    pub fn lane_eligible(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:eligible", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:delayed`
    pub fn lane_delayed(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:delayed", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:active`
    pub fn lane_active(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:active", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:terminal`
    pub fn lane_terminal(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:terminal", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies`
    pub fn lane_blocked_dependencies(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:blocked:dependencies", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:blocked:budget`
    pub fn lane_blocked_budget(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:blocked:budget", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:blocked:quota`
    pub fn lane_blocked_quota(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:blocked:quota", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:blocked:route`
    pub fn lane_blocked_route(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:blocked:route", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:blocked:operator`
    pub fn lane_blocked_operator(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:blocked:operator", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lane:<lane_id>:suspended`
    pub fn lane_suspended(&self, lane_id: &LaneId) -> String {
        format!("ff:idx:{}:lane:{}:suspended", self.tag, lane_id)
    }

    /// `ff:idx:{p:N}:lease_expiry` — Cross-lane lease expiry scanner target.
    pub fn lease_expiry(&self) -> String {
        format!("ff:idx:{}:lease_expiry", self.tag)
    }

    /// `ff:idx:{p:N}:worker:<worker_instance_id>:leases`
    pub fn worker_leases(&self, wid: &WorkerInstanceId) -> String {
        format!("ff:idx:{}:worker:{}:leases", self.tag, wid)
    }

    /// `ff:idx:{p:N}:suspension_timeout`
    pub fn suspension_timeout(&self) -> String {
        format!("ff:idx:{}:suspension_timeout", self.tag)
    }

    /// `ff:idx:{p:N}:pending_waitpoint_expiry`
    pub fn pending_waitpoint_expiry(&self) -> String {
        format!("ff:idx:{}:pending_waitpoint_expiry", self.tag)
    }

    /// `ff:idx:{p:N}:attempt_timeout`
    pub fn attempt_timeout(&self) -> String {
        format!("ff:idx:{}:attempt_timeout", self.tag)
    }

    /// `ff:idx:{p:N}:execution_deadline`
    pub fn execution_deadline(&self) -> String {
        format!("ff:idx:{}:execution_deadline", self.tag)
    }

    /// `ff:idx:{p:N}:all_executions`
    pub fn all_executions(&self) -> String {
        format!("ff:idx:{}:all_executions", self.tag)
    }
}

// ─── Flow Partition Keys ({fp:N}) ───

/// Key context for flow-structural partition keys.
pub struct FlowKeyContext {
    tag: String,
    fid: String,
}

impl FlowKeyContext {
    pub fn new(partition: &Partition, fid: &FlowId) -> Self {
        Self {
            tag: partition.hash_tag(),
            fid: fid.to_string(),
        }
    }

    /// `ff:flow:{fp:N}:<flow_id>:core`
    pub fn core(&self) -> String {
        format!("ff:flow:{}:{}:core", self.tag, self.fid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:members`
    pub fn members(&self) -> String {
        format!("ff:flow:{}:{}:members", self.tag, self.fid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:member:<eid>`
    pub fn member(&self, eid: &ExecutionId) -> String {
        format!("ff:flow:{}:{}:member:{}", self.tag, self.fid, eid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:edge:<edge_id>`
    pub fn edge(&self, edge_id: &EdgeId) -> String {
        format!("ff:flow:{}:{}:edge:{}", self.tag, self.fid, edge_id)
    }

    /// `ff:flow:{fp:N}:<flow_id>:out:<upstream_eid>`
    pub fn outgoing(&self, upstream_eid: &ExecutionId) -> String {
        format!("ff:flow:{}:{}:out:{}", self.tag, self.fid, upstream_eid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:in:<downstream_eid>`
    pub fn incoming(&self, downstream_eid: &ExecutionId) -> String {
        format!("ff:flow:{}:{}:in:{}", self.tag, self.fid, downstream_eid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:events`
    pub fn events(&self) -> String {
        format!("ff:flow:{}:{}:events", self.tag, self.fid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:summary`
    pub fn summary(&self) -> String {
        format!("ff:flow:{}:{}:summary", self.tag, self.fid)
    }

    /// `ff:flow:{fp:N}:<flow_id>:grant:<mutation_id>`
    pub fn grant(&self, mutation_id: &str) -> String {
        format!("ff:flow:{}:{}:grant:{}", self.tag, self.fid, mutation_id)
    }

    pub fn hash_tag(&self) -> &str {
        &self.tag
    }
}

/// Flow-partition index keys.
pub struct FlowIndexKeys {
    tag: String,
}

impl FlowIndexKeys {
    pub fn new(partition: &Partition) -> Self {
        Self {
            tag: partition.hash_tag(),
        }
    }

    /// `ff:idx:{fp:N}:flow_index` — SET of flow IDs on this partition.
    /// Used by the flow projector for cluster-safe discovery (replaces SCAN).
    pub fn flow_index(&self) -> String {
        format!("ff:idx:{}:flow_index", self.tag)
    }
}

// ─── Budget Partition Keys ({b:M}) ───

/// Key context for budget partition keys.
pub struct BudgetKeyContext {
    tag: String,
    bid: String,
}

impl BudgetKeyContext {
    pub fn new(partition: &Partition, bid: &BudgetId) -> Self {
        Self {
            tag: partition.hash_tag(),
            bid: bid.to_string(),
        }
    }

    /// `ff:budget:{b:M}:<budget_id>` — Budget definition.
    pub fn definition(&self) -> String {
        format!("ff:budget:{}:{}", self.tag, self.bid)
    }

    /// `ff:budget:{b:M}:<budget_id>:limits` — Hard/soft limits per dimension.
    pub fn limits(&self) -> String {
        format!("ff:budget:{}:{}:limits", self.tag, self.bid)
    }

    /// `ff:budget:{b:M}:<budget_id>:usage` — Usage counters.
    pub fn usage(&self) -> String {
        format!("ff:budget:{}:{}:usage", self.tag, self.bid)
    }

    /// `ff:budget:{b:M}:<budget_id>:executions` — Reverse index.
    pub fn executions(&self) -> String {
        format!("ff:budget:{}:{}:executions", self.tag, self.bid)
    }

    pub fn hash_tag(&self) -> &str {
        &self.tag
    }
}

/// Budget attachment key (not budget-ID scoped).
pub fn budget_attach_key(tag: &str, scope_type: &str, scope_id: &str) -> String {
    format!("ff:budget_attach:{}:{}:{}", tag, scope_type, scope_id)
}

/// Budget reset schedule index.
pub fn budget_resets_key(tag: &str) -> String {
    format!("ff:idx:{}:budget_resets", tag)
}

/// Budget policies index — SET of budget IDs on this partition.
/// Used by the budget reconciler for cluster-safe discovery (replaces SCAN).
pub fn budget_policies_index(tag: &str) -> String {
    format!("ff:idx:{}:budget_policies", tag)
}

// ─── Quota Partition Keys ({q:K}) ───

/// Key context for quota partition keys.
pub struct QuotaKeyContext {
    tag: String,
    qid: String,
}

impl QuotaKeyContext {
    pub fn new(partition: &Partition, qid: &QuotaPolicyId) -> Self {
        Self {
            tag: partition.hash_tag(),
            qid: qid.to_string(),
        }
    }

    /// `ff:quota:{q:K}:<quota_policy_id>` — Quota policy definition.
    pub fn definition(&self) -> String {
        format!("ff:quota:{}:{}", self.tag, self.qid)
    }

    /// `ff:quota:{q:K}:<quota_policy_id>:window:<dimension>`
    pub fn window(&self, dimension: &str) -> String {
        format!("ff:quota:{}:{}:window:{}", self.tag, self.qid, dimension)
    }

    /// `ff:quota:{q:K}:<quota_policy_id>:concurrency`
    pub fn concurrency(&self) -> String {
        format!("ff:quota:{}:{}:concurrency", self.tag, self.qid)
    }

    /// `ff:quota:{q:K}:<quota_policy_id>:admitted:<execution_id>`
    pub fn admitted(&self, eid: &ExecutionId) -> String {
        format!("ff:quota:{}:{}:admitted:{}", self.tag, self.qid, eid)
    }

    /// `ff:quota:{q:K}:<quota_policy_id>:admitted_set` — SET of admitted execution IDs.
    /// Used by the quota reconciler instead of SCAN (cluster-safe).
    pub fn admitted_set(&self) -> String {
        format!("ff:quota:{}:{}:admitted_set", self.tag, self.qid)
    }

    pub fn hash_tag(&self) -> &str {
        &self.tag
    }
}

/// Quota policy index — SET of policy IDs on this partition.
/// Used by the quota reconciler for cluster-safe discovery (replaces SCAN).
pub fn quota_policies_index(tag: &str) -> String {
    format!("ff:idx:{}:quota_policies", tag)
}

/// Quota attachment key.
pub fn quota_attach_key(tag: &str, scope_type: &str, scope_id: &str) -> String {
    format!("ff:quota_attach:{}:{}:{}", tag, scope_type, scope_id)
}

// ─── Global Keys (no hash tag) ───

/// Lane configuration key.
pub fn lane_config_key(lane_id: &LaneId) -> String {
    format!("ff:lane:{}:config", lane_id)
}

/// Lane counts key.
pub fn lane_counts_key(lane_id: &LaneId) -> String {
    format!("ff:lane:{}:counts", lane_id)
}

/// Worker registration key.
pub fn worker_key(wid: &WorkerInstanceId) -> String {
    format!("ff:worker:{}", wid)
}

/// Global worker index.
pub fn workers_index_key() -> String {
    "ff:idx:workers".to_owned()
}

/// Worker capability index.
pub fn workers_capability_key(key: &str, value: &str) -> String {
    format!("ff:idx:workers:cap:{}:{}", key, value)
}

/// Lane registry.
pub fn lanes_index_key() -> String {
    "ff:idx:lanes".to_owned()
}

/// Partition configuration — `ff:config:partitions` (§8.3).
/// Validated on startup; created on first boot.
pub fn global_config_partitions() -> String {
    "ff:config:partitions".to_owned()
}

// ─── Cross-Partition Secondary Indexes ───

/// `ff:ns:<namespace>:executions`
pub fn namespace_executions_key(namespace: &str) -> String {
    format!("ff:ns:{}:executions", namespace)
}

/// `ff:idem:{p:N}:<namespace>:<idempotency_key>`
///
/// Includes the execution partition hash tag so the key hashes to the same
/// Valkey cluster slot as all other KEYS in the ff_create_execution FCALL.
pub fn idempotency_key(tag: &str, namespace: &str, idem_key: &str) -> String {
    format!("ff:idem:{}:{}:{}", tag, namespace, idem_key)
}

/// Placeholder key that shares a hash tag with other KEYS in the same FCALL.
///
/// Used when an optional key (e.g. idempotency key) is absent. The Lua
/// function never reads/writes this key, but Valkey cluster requires all
/// KEYS in a single FCALL to hash to the same slot.
pub fn noop_key(tag: &str) -> String {
    format!("ff:noop:{}", tag)
}

/// `ff:tag:<namespace>:<key>:<value>`
pub fn tag_index_key(namespace: &str, key: &str, value: &str) -> String {
    format!("ff:tag:{}:{}:{}", namespace, key, value)
}

/// Waitpoint key resolution — `ff:wpkey:{p:N}:<waitpoint_key>`
pub fn waitpoint_key_resolution(tag: &str, wp_key: &WaitpointKey) -> String {
    format!("ff:wpkey:{}:{}", tag, wp_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::{execution_partition, flow_partition, PartitionConfig};

    #[test]
    fn exec_key_context_core_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::parse("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let core_key = ctx.core();
        assert!(core_key.starts_with("ff:exec:{p:"));
        assert!(core_key.ends_with(":core"));
        assert!(core_key.contains("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn exec_key_all_keys_share_hash_tag() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::new();
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);
        let tag = ctx.hash_tag();

        // All keys must contain the same hash tag
        assert!(ctx.core().contains(tag));
        assert!(ctx.payload().contains(tag));
        assert!(ctx.result().contains(tag));
        assert!(ctx.policy().contains(tag));
        assert!(ctx.lease_current().contains(tag));
        assert!(ctx.lease_history().contains(tag));
        assert!(ctx.attempts().contains(tag));
        assert!(ctx.suspension_current().contains(tag));
        assert!(ctx.exec_signals().contains(tag));
    }

    #[test]
    fn attempt_key_includes_index() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::new();
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let key = ctx.attempt_hash(AttemptIndex::new(3));
        assert!(key.contains(":3"), "attempt key should contain index");
    }

    #[test]
    fn stream_key_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::new();
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let key = ctx.stream(AttemptIndex::new(0));
        assert!(key.starts_with("ff:stream:{p:"));
        assert!(key.ends_with(":0"));
    }

    #[test]
    fn index_keys_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::new();
        let partition = execution_partition(&eid, &config);
        let idx = IndexKeys::new(&partition);
        let lane = LaneId::new("default");

        assert!(idx.lane_eligible(&lane).contains(":lane:default:eligible"));
        assert!(idx.lane_delayed(&lane).contains(":lane:default:delayed"));
        assert!(idx.lease_expiry().contains(":lease_expiry"));
        assert!(idx.all_executions().contains(":all_executions"));
    }

    #[test]
    fn flow_key_context_format() {
        let config = PartitionConfig::default();
        let fid = FlowId::new();
        let partition = flow_partition(&fid, &config);
        let ctx = FlowKeyContext::new(&partition, &fid);

        assert!(ctx.core().starts_with("ff:flow:{fp:"));
        assert!(ctx.core().ends_with(":core"));
        assert!(ctx.members().ends_with(":members"));
    }

    #[test]
    fn global_keys_no_hash_tag() {
        let lane = LaneId::new("default");
        let key = lane_config_key(&lane);
        assert_eq!(key, "ff:lane:default:config");
        assert!(!key.contains('{'));
    }
}
