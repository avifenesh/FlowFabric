use crate::partition::Partition;
use crate::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, QuotaPolicyId, SignalId,
    WaitpointId, WaitpointKey, WorkerInstanceId,
};

// ─── Execution Partition Keys ({fp:N} post-RFC-011) ───
//
// NOTE: docstrings below still reference `{p:N}` as a historical shorthand.
// Post-RFC-011 all exec-scoped keys hash-tag to `{fp:N}` (the parent flow's
// partition), enabling atomic multi-key FCALLs across exec_core + flow_core.
// The `Partition` type's `hash_tag()` method produces the correct `{fp:N}`
// prefix; the docstring example paths are indicative of structure, not the
// literal runtime hash-tag.

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

    /// `ff:attempt:{p:N}:<eid>:<attempt_index>:summary` — Rolling summary
    /// Hash for the `DurableSummary` stream mode (RFC-015 §3.1). Shares
    /// the `{p:N}` hash-tag slot with [`Self::stream`] / [`Self::stream_meta`]
    /// so the summary Hash, the stream key, and the stream-meta Hash are
    /// all co-located for atomic multi-key FCALL application from the
    /// single-shard Lua Function.
    pub fn stream_summary(&self, index: AttemptIndex) -> String {
        format!("ff:attempt:{}:{}:{}:summary", self.tag, self.eid, index)
    }

    // ── Suspension / Waitpoint (RFC-004) ──

    /// `ff:exec:{p:N}:<eid>:suspension:current` — Current suspension episode.
    pub fn suspension_current(&self) -> String {
        format!("ff:exec:{}:{}:suspension:current", self.tag, self.eid)
    }

    /// `ff:exec:{p:N}:<eid>:suspension:current:satisfied_set` — RFC-014
    /// §3.1. Durable SET of satisfier tokens accumulated during the
    /// active suspension. Created at `suspend_execution` (for composite
    /// conditions only) and deleted on the three terminating paths
    /// (resume, cancel, expire).
    pub fn suspension_satisfied_set(&self) -> String {
        format!(
            "ff:exec:{}:{}:suspension:current:satisfied_set",
            self.tag, self.eid
        )
    }

    /// `ff:exec:{p:N}:<eid>:suspension:current:member_map` — RFC-014
    /// §3.1. Write-once HASH mapping `waitpoint_id → node_path`
    /// (e.g. `"members[0]"`). Read by `deliver_signal` to locate which
    /// composite node a signal affects.
    pub fn suspension_member_map(&self) -> String {
        format!(
            "ff:exec:{}:{}:suspension:current:member_map",
            self.tag, self.eid
        )
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

    /// `ff:dedup:suspend:{p:N}:<eid>:<idem_key>` — Suspend idempotency
    /// guard (RFC-013 §2.2). Partition-scoped hash that stores a
    /// serialized [`crate::contracts::SuspendOutcome`] when a caller
    /// supplies `SuspendArgs::idempotency_key`; a retry within the TTL
    /// window replays the first outcome verbatim without state
    /// mutation.
    pub fn suspend_dedup(&self, idempotency_key: &str) -> String {
        format!(
            "ff:dedup:suspend:{}:{}:{}",
            self.tag, self.eid, idempotency_key
        )
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

    /// `ff:sec:{p:N}:waitpoint_hmac` — HMAC signing secrets replicated
    /// across every execution partition (RFC-004 §Waitpoint Security).
    /// Hash fields:
    ///   `current_kid`, `previous_kid`, `secret:<kid>` (hex), `previous_expires_at`.
    /// Replication is required for cluster mode: every FCALL that mints or
    /// validates a token must hash-tag-collocate this key with the rest of
    /// its execution-partition KEYS. The secret value is identical across
    /// partitions; rotation fans out HSETs across them.
    pub fn waitpoint_hmac_secrets(&self) -> String {
        format!("ff:sec:{}:waitpoint_hmac", self.tag)
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

    /// `ff:part:{fp:N}:signal_delivery` — partition-level aggregate
    /// stream that `ff_deliver_signal` XADDs into on every successful
    /// delivery. `subscribe_signal_delivery` XREAD BLOCKs this key.
    /// RFC-019 Stage B / #310.
    pub fn partition_signal_delivery(&self) -> String {
        format!("ff:part:{}:signal_delivery", self.tag)
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

    /// `ff:flow:{fp:N}:<flow_id>:tags` — User-supplied key-value labels.
    /// Symmetric with [`ExecKeyContext::tags`]. Populated by
    /// `ff_set_flow_tags`, which also lazy-migrates any pre-58.4
    /// reserved-namespace fields stashed inline on `flow_core`.
    pub fn tags(&self) -> String {
        format!("ff:flow:{}:{}:tags", self.tag, self.fid)
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

    /// `ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_eid>` — RFC-016
    /// Stage A edge-group hash. Fields written in Stage A:
    /// `policy_variant` (`all_of`), `n`, `succeeded`, `group_state`.
    /// Stage B+ adds `on_satisfied`, `failed`, `skipped`, `satisfied_at`,
    /// `cancel_siblings_pending`.
    pub fn edgegroup(&self, downstream_eid: &ExecutionId) -> String {
        format!(
            "ff:flow:{}:{}:edgegroup:{}",
            self.tag, self.fid, downstream_eid
        )
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

    /// `ff:flow:{fp:N}:<flow_id>:pending_cancels` — SET of execution IDs
    /// whose cancel is still owed after a `cancel_all` cancel_flow. The
    /// live async dispatch SREMs entries as it succeeds; the cancel
    /// reconciler scanner drains the remainder on its interval so a
    /// process crash mid-dispatch or a member whose cancel hit a
    /// permanent error can't leave a flow member un-cancelled.
    pub fn pending_cancels(&self) -> String {
        format!("ff:flow:{}:{}:pending_cancels", self.tag, self.fid)
    }

    pub fn hash_tag(&self) -> &str {
        &self.tag
    }

    /// The owning flow id as a string (as supplied to `new`).
    pub fn flow_id(&self) -> &str {
        &self.fid
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

    /// `ff:idx:{fp:N}:cancel_backlog` — ZSET of flow IDs whose async
    /// cancel dispatch is still owed members. Score = grace-window expiry
    /// (unix ms). The cancel reconciler scanner ZRANGEBYSCOREs entries
    /// whose score <= now, drains their `pending_cancels` set, and ZREMs
    /// when empty. Live dispatch runs without waiting on this score, so
    /// the grace window just keeps the reconciler from fighting the
    /// happy path.
    pub fn cancel_backlog(&self) -> String {
        format!("ff:idx:{}:cancel_backlog", self.tag)
    }

    /// `ff:idx:{fp:N}:pending_cancel_groups` — RFC-016 Stage C
    /// per-flow-partition SET of `<flow_id>|<downstream_eid>` tuples
    /// whose edgegroup has a non-empty sibling-cancel queue awaiting
    /// dispatch. Populated atomically by `ff_resolve_dependency` when
    /// the AnyOf/Quorum resolver flips to `satisfied|impossible` under
    /// `OnSatisfied::CancelRemaining`; drained by the dispatcher via
    /// `ff_drain_sibling_cancel_group` once per-sibling cancels have
    /// been acked. The sibling-cancel dispatcher scanner iterates this
    /// SET (cluster-safe) instead of scanning edgegroup hashes.
    pub fn pending_cancel_groups(&self) -> String {
        format!("ff:idx:{}:pending_cancel_groups", self.tag)
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

/// Non-authoritative capability advertisement STRING for a worker
/// (sorted CSV). Written by `ff-sdk::FlowFabricWorker::connect`, read by
/// the engine's unblock scanner to decide whether a `blocked_by_route`
/// execution has a matching worker. Cluster mode: the key lands on
/// whatever slot CRC16 hashes to — enumeration goes through
/// `workers_index_key()` rather than a keyspace SCAN, which would only
/// hit one shard in cluster mode.
pub fn worker_caps_key(wid: &WorkerInstanceId) -> String {
    format!("ff:worker:{}:caps", wid)
}

/// Global worker index — SET of connected worker_instance_ids. Single
/// slot in cluster mode (no hash tag; CRC16 of the literal key). SADD on
/// connect, SREM on empty-caps restart; SMEMBERS gives the enumerable
/// universe the unblock scanner walks. Separate from `ff:worker:{id}`
/// registration keys to keep the index membership cheap to read and
/// independent of per-worker hash details.
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

/// Shared prefix for the usage-dedup keyspace. Must match the
/// `ff:usagededup:` literal referenced in `lua/**.lua` (notably
/// `lua/budget.lua:99`, the `ff_report_usage_and_check` function).
/// Grep `ff:usagededup:` to find all producers, consumers, and test
/// fixtures in one search.
pub const USAGE_DEDUP_KEY_PREFIX: &str = "ff:usagededup:";

/// Build a usage-dedup key: `ff:usagededup:<hash_tag>:<dedup_id>`.
///
/// `hash_tag` must ALREADY include the Valkey hash-tag braces
/// (e.g. `"{bp:7}"`) — typically obtained from
/// [`BudgetKeyContext::hash_tag`]. Do not double-wrap.
pub fn usage_dedup_key(hash_tag: &str, dedup_id: &str) -> String {
    format!("{USAGE_DEDUP_KEY_PREFIX}{hash_tag}:{dedup_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::{execution_partition, flow_partition, PartitionConfig};

    #[test]
    fn exec_key_context_core_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::parse("{fp:0}:550e8400-e29b-41d4-a716-446655440000").unwrap();
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let core_key = ctx.core();
        // Post-RFC-011: exec keys co-locate on flow partitions ({fp:*}).
        assert!(core_key.starts_with("ff:exec:{fp:"));
        assert!(core_key.ends_with(":core"));
        assert!(core_key.contains("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn exec_key_all_keys_share_hash_tag() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
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
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let key = ctx.attempt_hash(AttemptIndex::new(3));
        assert!(key.contains(":3"), "attempt key should contain index");
    }

    #[test]
    fn stream_key_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let partition = execution_partition(&eid, &config);
        let ctx = ExecKeyContext::new(&partition, &eid);

        let key = ctx.stream(AttemptIndex::new(0));
        assert!(key.starts_with("ff:stream:{fp:"));
        assert!(key.ends_with(":0"));
    }

    #[test]
    fn index_keys_format() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
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

    #[test]
    fn usage_dedup_key_format() {
        // hash_tag already includes braces — helper must not double-wrap.
        let key = usage_dedup_key("{bp:7}", "dedup-123");
        assert_eq!(key, "ff:usagededup:{bp:7}:dedup-123");
        assert!(key.starts_with(USAGE_DEDUP_KEY_PREFIX));
        // Exactly one `{…}` hash-tag region.
        assert_eq!(key.matches('{').count(), 1);
        assert_eq!(key.matches('}').count(), 1);
    }
}
