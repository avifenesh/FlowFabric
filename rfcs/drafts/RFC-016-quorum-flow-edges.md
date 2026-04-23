# RFC-016: Any-of / quorum dependencies on flow edges

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-23
**Pre-RFC Reference:** RFC-007 §Dependency Model (lines 196-201, 854-860 — explicit deferral of any-of / quorum / threshold joins)
**Related RFCs:** RFC-007 (flow DAG + dependency semantics, baseline), RFC-001 (execution lifecycle + cancel), RFC-014 (waitpoint-level multi-signal — DIFFERENT primitive, see §8.2), RFC-010 (Valkey architecture, projector + partitioning)

---

## Summary

RFC-007 edges are `all_required` over `success_only` dependencies: a downstream execution becomes eligible only when **every** required inbound upstream reaches a successful terminal state, and **any** upstream skip or fail marks the downstream `skipped`. This RFC introduces a per-edge-group **`EdgeDependencyPolicy`** with three variants — `AllOf` (today's behavior), `AnyOf`, `Quorum { k }` — and a per-group **`OnSatisfied`** policy governing what happens to unfinished sibling upstreams once the quorum is met: `CancelRemaining` (default; save work) or `LetRun` (let unfinished siblings continue; useful for human approvals and scatter-gather analytics). It specifies the edge-group state machine, replay durability, wire format, and observability. Threshold / weighted joins are **explicitly out of scope** and rejected in §10 as a future RFC.

## Motivation

Two distinct real-world patterns cannot be expressed with `all_required` edges:

1. **LLM consensus / redundant compute.** Fan out the same prompt to N models (or N tool-use attempts) and proceed as soon as K of them return successfully. Once the K-th answer lands, the remaining attempts are waste — they burn budget, inference seconds, and downstream token-usage quota. Canonical shapes: `any_of(3)` (race to first), `quorum(3 of 5)` (majority consensus).

2. **Human approval / multi-reviewer sign-off.** Request review from N reviewers; proceed once K have approved. Once K approvals land, we do **not** want to rescind the outstanding review requests — the reviewers are already engaged, the UX damage of a rescinded request is real, and partial data (reviewer notes, flags) retains value post-decision.

Both patterns are quorum-shaped, but diverge on what to do with **unfinished siblings** after the quorum is met. That divergence is the core design axis of this RFC.

Secondary motivation: `any_of` is routinely requested for racy fanout (try three mirrors, take the first that succeeds). `AnyOf` is a ubiquitous spelling; even though it is mathematically `Quorum { k: 1 }`, keeping it as a named convenience avoids making simple use look exotic.

Non-goals for this RFC:

- Weighted / threshold joins (`approve if weighted_sum >= threshold`). Different design dimension — see §10.3.
- Waitpoint-level multi-signal aggregation within a single suspended execution. That is RFC-014's domain; see §8.2 for the per-edge vs per-waitpoint distinction.
- Changing `success_only` dependency semantics. A `Quorum { k }` still counts a terminal successful outcome as a "positive" upstream; non-success terminals count as "negative."

## Detailed Design

### §2.1. Worked examples

LLM-consensus (3-of-5 voters, cancel the losers once consensus is reached):

```rust
let voters: Vec<ExecutionId> =
    (0..5).map(|_| create_child_execution(flow_id, "vote", ...)).collect();
let decide = create_child_execution(flow_id, "decide", ...);

set_edge_group_policy(
    flow_id,
    decide,
    EdgeDependencyPolicy::Quorum { k: 3, on_satisfied: OnSatisfied::CancelRemaining },
);

for v in &voters {
    add_dependency(flow_id, *v, decide, EdgeSpec::default());
}
```

Human-approval (2-of-5 reviewers, let stragglers finish for audit trail):

```rust
let reviewers: Vec<ExecutionId> = request_reviews(...);
let approved = create_child_execution(flow_id, "approved", ...);

set_edge_group_policy(
    flow_id,
    approved,
    EdgeDependencyPolicy::Quorum { k: 2, on_satisfied: OnSatisfied::LetRun },
);

for r in &reviewers {
    add_dependency(flow_id, *r, approved, EdgeSpec::default());
}
```

Racy-fanout (`AnyOf`, cancel the losers):

```rust
set_edge_group_policy(
    flow_id,
    merged,
    EdgeDependencyPolicy::AnyOf { on_satisfied: OnSatisfied::CancelRemaining },
);
```

### §2. `EdgeDependencyPolicy` shape

Edges in RFC-007 are grouped by `downstream_execution_id`: a downstream execution has one **inbound edge group**, and the satisfaction condition was previously `all_required` across that group. RFC-016 elevates that satisfaction condition to a per-group policy.

```
enum EdgeDependencyPolicy {
    /// Today's behavior: every edge in the inbound group must be satisfied.
    /// Equivalent to the RFC-007 `all_required` + `success_only` pairing.
    AllOf,

    /// k-of-n where k==1. Convenience spelling; equivalent to
    /// `Quorum { k: 1, on_satisfied }`. Kept as a named variant because
    /// `any_of` is a ubiquitous concept and `Quorum { k: 1 }` reads awkwardly
    /// in flow definitions.
    AnyOf { on_satisfied: OnSatisfied },

    /// k-of-n successful upstreams satisfy the downstream.
    /// Requires k >= 1 and k <= n at flow-creation / edge-staging time.
    Quorum { k: u32, on_satisfied: OnSatisfied },
}

enum OnSatisfied {
    /// Default. When the quorum is met, cancel any still-running upstream
    /// siblings in the same inbound group via per-execution cancel with
    /// reason `sibling_quorum_satisfied`.
    CancelRemaining,

    /// When the quorum is met, leave still-running siblings alone. Their
    /// eventual terminal outcomes update the edge-group counters for
    /// observability but never flip downstream eligibility (one-shot).
    LetRun,
}
```

**Decision: keep `AnyOf` as a named variant, not collapsed into `Quorum { k: 1 }`.** Rationale: (a) `any_of` is a domain term from async primitives that users expect to see by name; (b) the engine can short-circuit `AnyOf` on the first success without walking counters; (c) the wire format cost of the extra discriminant is one byte. Rejected collapse: see §10.1.

**Scope of a "group."** A policy is declared once per downstream execution's inbound edge group. Mixing `AllOf` and `AnyOf` edges into the same downstream's group is **not supported** in RFC-016 (see §8.3 and §10.2). Per-edge policies within a group require threshold / weighted semantics, which are out of scope.

### §3. Edge-group state machine

Today, a single counter per downstream tracks "how many upstream edges resolved." RFC-016 replaces that with a four-counter state plus a frozen `n`.

Per downstream inbound edge group:

| Field | Type | Semantics |
| --- | --- | --- |
| `policy` | `EdgeDependencyPolicy` | Frozen at edge-group staging. |
| `n` | u32 | Total inbound edges in the group at staging time. |
| `succeeded` | u32 | Upstream terminal = success. |
| `failed` | u32 | Upstream terminal = failed (error, timeout, cancelled, attempt-exhausted). Sparse: see storage note. |
| `skipped` | u32 | Upstream terminal = skipped (skip-propagation from further upstream). Sparse: see storage note. |
| `running` | u32 | Derived: `n - (succeeded + failed + skipped)`. Not stored; computed. |
| `group_state` | enum | `pending` → `satisfied` \| `impossible` \| `cancelled`. One-shot. |
| `satisfied_at` | unix ms, nullable | When quorum was first met (for `LetRun` observability). Sparse. |
| `cancel_siblings_pending` | list\<execution_id\> | Populated atomically on `satisfied` when `OnSatisfied::CancelRemaining` and `running > 0`; drained by the dispatcher (§8.5). Empty for `LetRun` and for `AllOf`. |

**Storage sparseness (resolves M.D1).** `AllOf` groups maintain only `policy_variant`, `n`, `succeeded`, `group_state` — the 99% case pays close to today's per-downstream footprint. `failed`, `skipped`, `satisfied_at`, `cancel_siblings_pending` are written only by `AnyOf`/`Quorum` policies. Resolvers read whichever fields their policy requires.

**Transitions.** Evaluated atomically on the downstream's partition inside `ff_resolve_dependency` (RFC-007 line 398, 623) each time an upstream terminal lands:

1. On upstream terminal: increment the matching counter (`succeeded` / `failed` / `skipped`).
2. If `group_state != pending`, stop (one-shot; already fired). Counters still update for observability — this means `succeeded + failed + skipped` may reach `n` even after `group_state = impossible` (or `satisfied` under `LetRun`). Impossibility / satisfaction is one-shot; counters are not. Operator-facing docs MUST note this so `AllOf` snapshots showing `succeeded=3, failed=1, group_state=impossible` are not read as bugs.
3. Evaluate satisfaction against `policy`:
   - `AllOf`: satisfied iff `succeeded == n`; impossible iff `failed + skipped >= 1`.
   - `AnyOf { .. }`: satisfied iff `succeeded >= 1`; impossible iff `failed + skipped == n`.
   - `Quorum { k, .. }`: satisfied iff `succeeded >= k`; impossible iff `failed + skipped > n - k` (i.e., the remaining `running` upstreams cannot lift `succeeded` to `k` even if all succeed).
4. On satisfied: set `group_state = satisfied`, `satisfied_at = now`, transition downstream `eligibility_state: blocked_by_dependencies → eligible_now`, and (if policy carries `OnSatisfied::CancelRemaining` and `running > 0`) enqueue per-sibling cancel (see §4).
5. On impossible: set `group_state = impossible`, transition downstream to `terminal_outcome = skipped` (RFC-007 skip-propagation), and cascade-resolve the downstream's outbound edges.

**Invariant Q1.** `group_state` transitions are one-shot: once `satisfied` or `impossible`, no further transition. Later upstream terminals update counters but do not re-fire eligibility or re-skip the downstream.

**Invariant Q2.** `succeeded + failed + skipped <= n` at all times. The Lua resolver MUST reject counter increments that would exceed this (dedup key: `edge_id`; each edge resolves exactly once).

**Invariant Q3.** Short-circuit impossibility fires as soon as `failed + skipped > n - k` regardless of how many upstreams are still running. Rationale: running the last sibling is wasted work; short-circuit matches the "cost-conscious" intent of the default. Under `OnSatisfied::CancelRemaining`, the short-circuit ALSO issues sibling cancels (same cancel machinery as §4, but reason is `sibling_quorum_impossible`). Under `OnSatisfied::LetRun`, the short-circuit skips the downstream but leaves siblings running — consistent with LetRun's "don't rescind" semantics.

**Invariant Q4.** `AllOf` short-circuits to impossible on the **first** non-success terminal (today's behavior). No change for backward-compat.

### §4. `CancelRemaining` semantics

When `group_state` flips to `satisfied` under `OnSatisfied::CancelRemaining` with `running > 0`, the engine issues a cancel to each still-running upstream sibling in the group.

**Cancel mechanism.** RFC-007's `cancel_flow` cancels a whole flow; it is not the right tool here. RFC-016 requires a **per-execution cancel with reason**:

```
cancel_execution(execution_id, reason: CancelReason)
```

where `CancelReason` is an enum with at least:

- `operator_cancel` (today's default for `cancel_flow` members)
- `flow_cancelled` (cascade from `cancel_flow`)
- `sibling_quorum_satisfied` (RFC-016: quorum met, this sibling is now redundant)
- `sibling_quorum_impossible` (RFC-016: quorum can't be reached; §3 short-circuit)

The reason is durably stored on the execution's terminal record so retry policies, observability, and replay can distinguish "operator pulled the plug" from "engine decided this work was redundant." Retry policies SHOULD treat `sibling_quorum_satisfied` as non-retriable at the flow level — it is a successful engine decision, not a failure.

**Dispatch mechanism.** For each sibling with non-terminal lifecycle, the engine resolver returns a `cancel_siblings: [execution_id, ...]` payload (same partition-local resolver function). The ff-engine dispatcher issues `cancel_execution` per sibling (cross-partition via `{p:N}` as needed), identical to the cascade pattern RFC-007 line 885 already uses for skipped children.

**Race: sibling completes between "quorum met" decision and "cancel signal arrives."**

- If the sibling has already reached a terminal state before the cancel lands: the cancel is a **no-op** at the execution partition. The resolver logs at `info` level (`cancel_sibling_ignored_terminal`), does NOT raise an error, and the sibling's terminal state still updates the (now-post-satisfied) counters for observability.
- If the sibling has already been claimed and is executing: the cancel signals the worker through the existing cancellation path (RFC-001 lease-cancel signal), and when the worker surfaces a terminal, that terminal reports `terminal_outcome = cancelled` with `cancel_reason = sibling_quorum_satisfied`.
- If the sibling is still `blocked_by_dependencies` or `eligible_now` but unclaimed: cancel transitions it directly to `terminal_outcome = cancelled` without a worker round-trip.

**Invariant Q5.** Cancels issued under `sibling_quorum_satisfied` never fail the parent flow. They are a first-class "successful resolution" outcome. The flow failure policy (RFC-007) MUST treat this cancel reason as benign.

**Invariant Q6.** `cancel_siblings_pending` is empty iff every non-terminal sibling at `satisfied_at` has been dispatched exactly one `cancel_execution`. Populated atomically with the `satisfied` flip; drained atomically per dispatch ack (§8.5).

### §4.1. Cancel dispatch batching (resolves M.D2)

High-fanout groups (e.g., `quorum(1 of 1000)`) must not emit 999 individual cross-partition cancels in a single dispatcher tick. The resolver groups sibling cancels by partition:

```
cancel_siblings_by_partition: Map<PartitionId, Vec<ExecutionId>>
```

The dispatcher issues one Lua call per partition: `ff_cancel_executions_batch(ids, reason)`. A **soft advisory cap** of 128 siblings per group emits `ff_edge_group_sibling_cancel_soft_cap_exceeded_total` when exceeded; no hard cap in v1. The Stage C gate benchmark (see §11) MUST measure per-partition batch latency at N = 32, 128, 512, 1024 before Stage C ships.

**Dispatch contract (resolves K.D6).** `ff_cancel_executions_batch(ids, reason)` returns a per-id disposition list: `[(execution_id, cancelled | already_terminal | not_found), ...]`. The dispatcher acks both `cancelled` AND `already_terminal` entries via `ff_ack_sibling_cancel` — both are successful resolutions (the cancel signal either was delivered or did not need to be). `not_found` entries are logged and NOT acked; they remain in `cancel_siblings_pending` for reconciler retry, covering projector-lag cases where the sibling execution record arrives late. A Lua error aborts the whole batch atomically; no acks are issued; the reconciler re-issues on its next tick.

**Dispatcher-side coalescing (resolves M.D6).** The ff-engine cancel dispatcher SHOULD coalesce `cancel_siblings_by_partition` entries across groups within a bounded window (implementation guidance: ~10ms, tunable) so that N concurrent satisfied groups targeting the same partition result in O(partitions-touched) Lua calls, not O(groups × partitions-per-group). This is a dispatcher concern, not a Lua surface change. The Stage C benchmark (§11) MUST exercise both the single-group-fan-out case (N = 32/128/512/1024 siblings on one group) AND the concurrent-groups case (≥500 groups satisfying within a ~100ms window).

### §5. `LetRun` semantics

Under `OnSatisfied::LetRun`, when `group_state` flips to `satisfied`, no cancels are issued. Still-running siblings continue to terminal. Their terminal outcomes update the counters for observability (§3 step 1) but, because of Invariant Q1, never retrigger downstream eligibility — the downstream fires exactly once.

**`LetRun` under `impossible`.** When `group_state` flips to `impossible` (Q3 short-circuit) under `LetRun`, the downstream is marked `skipped` and skip-propagation runs as today (§3 step 5), but still-running siblings are NOT cancelled — consistent with `LetRun`'s one-shot, no-rescind contract. Late sibling terminals continue to update counters for observability. `LetRun` is symmetric across both terminal `group_state` branches: no cancels, ever.

**Why one-shot, not re-evaluate.** A re-evaluating model would mean a downstream could fire, complete, and then be "re-fired" when a late sibling terminal pushes `succeeded` higher. That is meaningless in the flow DAG (a completed execution cannot become "more eligible"); and for observability it conflates "satisfied" with "still gathering data." One-shot matches the intuition: the downstream decision was made at `satisfied_at`; anything after is telemetry.

**Late-arriving success under impossible.** If `group_state = impossible` and a straggler sibling later reports `succeeded`, counters update but the downstream remains `skipped`. Once impossible, always impossible (Invariant Q1 + Q4). This is consistent with RFC-007's existing "no un-skipping."

**Data-passing under LetRun.** RFC-007 `data_passing_ref` is metadata only. When a downstream fires at `satisfied_at` under `LetRun`, it observes the upstream outputs for the `succeeded` upstreams known **at that moment**. Late-arriving sibling outputs are NOT re-delivered to the downstream. Consumers that need "all reviewer notes even the late ones" must either use `AllOf` or poll sibling outputs explicitly via `get_flow_graph`.

### §6. Flow API + wire format

#### §6.1 Edge staging

RFC-007 `add_dependency(flow_id, upstream, downstream, edge_spec)` stages one edge. RFC-016 extends `edge_spec` with an optional policy discriminant, but the policy lives on the **downstream's edge group**, not the individual edge. Two approaches were considered:

- **A.** Per-edge `policy` field; engine validates all edges in a group share the same policy. Rejected — redundant, error-prone on graph mutation.
- **B.** Declare the group policy separately via a new op `set_edge_group_policy(flow_id, downstream_execution_id, policy)`. **Accepted.**

New operation (Class A, atomic on downstream partition):

```
set_edge_group_policy(flow_id, downstream_execution_id, policy: EdgeDependencyPolicy)
```

Semantics:

- Must be called BEFORE the first `add_dependency(..., downstream_execution_id, ...)` call for this downstream. This is a strict, mechanically-enforceable ordering rule: `add_dependency` checks group existence and rejects policy change after its first call. Resolves K.D4 by picking option (a) — ordering is keyed on `add_dependency` entry, not on "first resolver run," avoiding the surprise path where adding a dependency on an already-terminal upstream implicitly activates a default-`AllOf` group.
- Default if never called: `EdgeDependencyPolicy::AllOf`. Existing flows created pre-RFC-016 are implicitly `AllOf` — full backward compatibility.
- `add_dependency` does NOT take a policy parameter (resolves L.D2, option a). The sole entry point for setting policy is `set_edge_group_policy`. Higher-level SDK helpers (e.g., `add_quorum_group(flow_id, downstream, upstreams, policy)`) MAY wrap both calls; they are additive convenience, not engine surface. Stage B (§11) is updated accordingly — the "optional group-policy parameter on `add_dependency`" proposal is struck.
- For `Quorum { k }`, the final `k` must satisfy `1 <= k <= n` where `n` is `edge_count` on the downstream group at `group_state` evaluation time. If the flow uses dynamic expansion (`dynamic_expansion_enabled`), `n` is measured each time the resolver runs; if the group still has not reached `k` edges when an upstream resolves, the resolver simply waits (same as today's `AllOf` waiting for more edges to stage) — see §8.4 for the explicit `k > current_n` tolerance and stuck-group observability.
- Errors: `invalid_policy` (k < 1), `policy_already_set` (attempt to change after first `add_dependency`), `group_policy_fixed_after_activation` (any edge in group already resolved).

#### §6.2 `EdgeSnapshot` additions

`EdgeSnapshot` (crates/ff-sdk snapshot.rs, ff-core decode) is the observer-facing per-edge view. RFC-016 does NOT add per-edge policy there — policy is per-group. Instead, add a sibling snapshot:

```
struct EdgeGroupSnapshot {
    flow_id: Uuid,
    downstream_execution_id: Uuid,
    policy: EdgeDependencyPolicy,
    n: u32,
    succeeded: u32,
    failed: u32,
    skipped: u32,
    group_state: EdgeGroupState,   // pending | satisfied | impossible | cancelled
    satisfied_at: Option<u64>,
}
```

Exposed via a new SDK read: `get_edge_group(flow_id, downstream_execution_id) -> Option<EdgeGroupSnapshot>`, and `describe_flow` gains a `edge_groups: Vec<EdgeGroupSnapshot>` field.

**Lookup ergonomics (resolves L.D3).** `describe_flow` is the canonical listing; `get_edge_group(flow_id, downstream_execution_id)` is the direct-lookup primitive keyed by execution id. Name-based helpers (e.g., "find the approval step by step-name") are SDK-layer concerns and are out of scope for this RFC — SDKs are free to layer such helpers on top of `describe_flow`.

`EdgeSnapshot` itself is unchanged. Per-edge identity, upstream/downstream pointer, and `edge_state` (pending/satisfied/impossible/cancelled) all keep their RFC-007 meanings.

#### §6.3 Wire format (Valkey)

RFC-007 line 453 defines edge keys. RFC-016 adds:

- `ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_execution_id>` — hash. Field set is sparse (§3 storage sparseness): `AllOf` writes only `policy_variant`, `n`, `succeeded`, `group_state`; `AnyOf`/`Quorum` additionally write `on_satisfied`, `failed`, `skipped`, `satisfied_at`, and (under `CancelRemaining`) `cancel_siblings_pending`.
- `ff:pending_cancel_groups:{p:N}` — per-partition SET of `downstream_execution_id` values whose edge group has `cancel_siblings_pending` non-empty on partition `p:N`. Populated atomically by `ff_resolve_dependency` when flipping `satisfied` with `CancelRemaining` and `running > 0`; drained atomically by `ff_ack_sibling_cancel` when the pending list empties. The reconciler iterates this SET per partition — never a full hash-scan. Aligns with the project-wide "SCAN → SETs" stance.
- `policy_variant` values: `all_of` | `any_of` | `quorum` (string, forward-compatible with future additions).

**`policy_variant` string vs. numeric trade (resolves M.D3, partial).** The string form is retained in v1 for debuggability — raw `HGETALL` output against the group hash is self-describing, which pays off in incident response. The per-group string cost (~7-8 bytes vs. 1 byte numeric tag) is acknowledged as a deliberate trade. If large-scale deployments surface this as a meaningful storage pressure, a future RFC may introduce a compact numeric code with a backwards-compatible dual-read path; the forward-compat contract in §6.4 already supports that migration without re-signing the hash format.

`ff_resolve_dependency` reads the group hash, increments the counter, evaluates satisfaction, and returns an action record `{ eligibility: satisfied|impossible|pending, cancel_siblings_by_partition: Map<PartitionId, Vec<ExecutionId>> }` (§4.1). The dispatcher side is unchanged except it honors the new partition-batched `cancel_siblings_by_partition` map alongside the existing `child_skipped` cascade.

#### §6.4 Forward-compat

`policy_variant` is a string, not a bit flag. If a future RFC adds `Threshold` (see §10.3), existing engines that see an unknown variant MUST return `unsupported_policy_variant` and refuse to resolve the group — fail loud, never silently misinterpret. Projection layers / SDK older than the engine MUST surface the raw variant string rather than crash on enum-decode.

### §7. Observability

New metrics (Prometheus labels follow RFC-010 conventions):

- `ff_edge_group_policy_total{policy}` — gauge of active groups by policy (sampled from projector).
- `ff_edge_group_evaluation_total{policy, outcome}` — counter of resolver evaluations. `outcome ∈ { pending, satisfied, impossible }`.
- `ff_edge_group_sibling_cancel_total{reason}` — counter of sibling cancels issued. `reason ∈ { sibling_quorum_satisfied, sibling_quorum_impossible }`.
- `ff_edge_group_let_run_late_terminal_total{terminal}` — counter of terminals arriving after `satisfied` under `LetRun`. `terminal ∈ { success, failed, skipped, cancelled }`. Useful to detect budget waste under mis-tuned `LetRun`.
- `ff_edge_group_sibling_cancel_soft_cap_exceeded_total` — counter (unlabelled) of groups whose `cancel_siblings_pending` exceeded the 128 advisory soft cap (§4.1).
- `ff_edge_group_evaluation_total{policy, outcome, reason}` adds an optional `reason` label for `outcome=pending` cases: `reason ∈ { k_exceeds_n, awaiting_upstream }` (resolves K.D5 — stuck-waiting `Quorum` groups during dynamic expansion are visible).

**Label-set commitment.** Metric labels are bounded to `policy`, `outcome`, `reason`, `terminal`. `flow_id`, `execution_id`, and user-assigned names are NEVER metric labels in v1 (resolves M.D4). Per-flow quorum diagnostics live in `describe_flow.edge_groups` snapshots, not metrics.

`describe_flow` additions:

- `edge_group_count_by_policy: { all_of, any_of, quorum }`
- `edge_groups: Vec<EdgeGroupSnapshot>` (bounded; paginated if > 256)

Tracing: `ff_resolve_dependency` span gets `policy`, `k`, `succeeded/n`, and `outcome` attributes.

### §8. Interactions with other primitives

#### §8.1 RFC-007 baseline

RFC-007's `all_required` + `success_only` model is preserved as `EdgeDependencyPolicy::AllOf`. All existing flows remain bit-identical in behavior. RFC-007 §Dependency Model should be updated in a follow-up amendment to reference this RFC as the successor for the "designed for later" bullets at lines 196-201 and 854-860.

RFC-007 skip-propagation rules restated under RFC-016:

- **Under `AllOf`:** any upstream non-success terminal (`failed` | `skipped`) still immediately marks the downstream impossible → `skipped`. Unchanged.
- **Under `AnyOf` / `Quorum`:** a non-success terminal is an **input to the counter**, not a terminal signal on its own. Only when `failed + skipped > n - k` (with `k = 1` for `AnyOf`) does the downstream flip to `skipped`. This is the core semantic shift of the RFC: skip is no longer fatal to the group, it is a negative vote.

#### §8.2 RFC-014 (waitpoint-level multi-signal) — DIFFERENT primitive

**Edges coordinate executions; waitpoints coordinate signals within a suspended execution's wait window.** Do not conflate.

| Axis | RFC-016 (edges) | RFC-014 (waitpoints) |
| --- | --- | --- |
| Unit of coordination | Flow DAG nodes (executions) | Signals within one execution's suspend |
| Participants | Upstream sibling executions (distinct lifecycles) | Signal deliveries (no execution identity) |
| "Satisfied" means | Downstream execution becomes eligible-to-claim | Suspended execution resumes |
| Cancel concept | Cancel sibling executions (§4) | Drain / ignore pending signals |
| Scope | Flow-level | Execution-local |

An LLM-consensus workflow uses **RFC-016** (N sibling attempt-executions, quorum on their success). A human-approval workflow where one coordinator execution suspends and waits for approval signals on one waitpoint uses **RFC-014**. A workflow that spawns N reviewer-tasks as sibling executions and proceeds on quorum-approval uses **RFC-016**. Choice is by architectural shape, not a gradient.

§8.2 MUST be called out in `describe_flow` docs so operators reaching for "any-of" pick the correct primitive.

#### §8.3 Mixed-policy groups — not supported

An inbound edge group has exactly one policy. Mixing `AllOf` edges and `AnyOf` edges into the same downstream's inbound group is **rejected** at `set_edge_group_policy` time (there is no per-edge policy). The common pattern "2 mandatory + any-of 1-of-3 optional" must be modeled as a two-layer DAG: the 1-of-3 becomes a sibling "selector" execution whose terminal state feeds a single edge into the downstream. See §10.2.

#### §8.4 Dynamic expansion (RFC-007 §Dynamic expansion)

When `dynamic_expansion_enabled = true`, edges may be added after group policy is set but before the group activates. Rules:

- `n` is read at resolution time, not frozen at policy-set time.
- Adding an edge to a group whose `group_state != pending` is rejected with `group_already_terminal`.
- `Quorum { k }` validation (`k <= n`) is enforced at each edge add AND at each resolve evaluation. If edges are removed (not in v1, but designed for), an evaluation where `k > n` transitions to `impossible`.
- **Transient `k > current_n` tolerance (resolves K.D5).** A group where `k > current_n` at resolve time is treated as `pending` (waiting for more edges to stage), not as an error. This matches today's `AllOf` "wait for more edges" behavior. Flows that want strict `k-of-N` declaration with N known up-front must either (i) use static edge staging (no dynamic expansion for this group), OR (ii) call `set_edge_group_policy` AFTER all edges are staged — at which point `k > n` raises `invalid_policy` synchronously. Stuck-waiting groups are observable via `ff_edge_group_evaluation_total{outcome=pending, reason=k_exceeds_n}` (§7).

#### §8.5 Replay (RFC-001 + RFC-007)

If the engine crashes between "resolver decides satisfied" and "downstream eligibility flip + sibling cancel dispatch," replay must be idempotent. Guarantees:

1. `group_state`, `succeeded/failed/skipped`, `satisfied_at`, and `cancel_siblings_pending` are stored in the group hash on the downstream's `{p:N}` partition within the same atomic Lua `ff_resolve_dependency` call that reads them. Counter increment + satisfaction decision + downstream-eligibility flip + `cancel_siblings_pending` population is one atomic transaction.
2. Sibling cancel dispatch is a **separate** step performed by ff-engine after Lua returns. The resolver persists `cancel_siblings_pending: [execution_id, ...]` into the group hash within the same atomic transaction as the `satisfied` flip, AND adds the `downstream_execution_id` to the per-partition index SET `ff:pending_cancel_groups:{p:N}` (§6.3). (This corrects the earlier "recompute from live sibling state" description — Lua on the downstream's partition cannot read sibling execution lifecycle atomically, since siblings live on their own `{p:N}` partitions.) The dispatcher drains `cancel_siblings_pending` and issues `ff_cancel_executions_batch` per partition (§4.1); each successful batch dispatch removes the acked ids from the field via atomic Lua `ff_ack_sibling_cancel`. When `cancel_siblings_pending` becomes empty, the same Lua removes the `downstream_execution_id` from `ff:pending_cancel_groups:{p:N}`. On engine crash mid-dispatch, recovery is driven by the `ff_reconcile_pending_sibling_cancels` reconciler (§11 Stage C) — on boot AND periodically (~30s) it iterates `ff:pending_cancel_groups:{p:N}` per partition (O(groups-needing-retry), NOT a full scan) and re-issues. In the common no-crash case the SET is empty or near-empty. Duplicate cancels against already-terminal siblings are no-ops (§4 race handling). Invariant Q6 holds end-to-end.
3. Downstream eligibility flip is idempotent: transitioning an execution from `eligible_now` to `eligible_now` is a no-op in RFC-001.
4. `satisfied_at` is set once (conditional Lua update: `HSETNX` equivalent). Replay does not overwrite it.

### §9. Open questions

1. **`AnyOf { on_satisfied }` with `LetRun`: is this a real use case, or only `AnyOf { CancelRemaining }`?** Racy-mirror use cases want cancel. Human approval with `k=1` is unusual ("any reviewer approves, skip the rest") but plausible. Leaning: keep `LetRun` on `AnyOf` for symmetry; revisit if no consumer hits it within 6 months.
2. **Should `sibling_quorum_impossible` also count as a cancel under `LetRun`?** Today §3 says no — `LetRun` means "don't cancel, ever." But if the group is impossible, siblings will only produce waste. Counter-argument: the user picked `LetRun` knowing some siblings are "for the record." Lean: keep LetRun semantics pure — never cancel under LetRun, even on impossible. Revisit with consumer input.
3. **Cross-partition cost of `ff_edge_group_sibling_cancel_total` at high fanout.** For `quorum(1 of 1000)` (hypothetical stress shape), the first success triggers 999 cross-partition cancels. Mitigation is specified in §4.1 (partition-batched `ff_cancel_executions_batch`). The Stage C gate benchmark (§11) measures per-partition latency at N = 32/128/512/1024 as a ship-blocking requirement. A hard `n` cap is still deferred until that benchmark data exists; the advisory soft cap (128) is an observability hook, not an enforcement point.

### §10. Alternatives rejected

#### §10.1 Collapse `AnyOf` into `Quorum { k: 1 }`

Rejected. Arguments for collapse: fewer enum variants, one less code path. Arguments against (winning):

- `any_of` is a first-class async-primitive term; users expect it by name.
- `Quorum { k: 1 }` reads awkwardly in flow-definition code and observability output.
- The engine can short-circuit `AnyOf` on first success without walking the full counter-evaluation path (minor perf).
- Wire-format cost of the extra variant discriminant is one byte.

#### §10.2 Per-edge policy (heterogeneous groups)

Rejected. A "2 mandatory + any-of 1-of-3 optional" join is expressible as a two-layer DAG (a sibling "selector" execution representing the any-of branch feeds one edge into the downstream), and that explicit decomposition is clearer in the flow graph and simpler in the resolver. Per-edge policies add significant state-machine complexity (how to combine them? threshold weights?) for a use case that two-layer decomposition already solves.

#### §10.3 Threshold / weighted joins

**Explicitly out of scope.** Weighted quorum (`approve if sum(edge_weight * success) >= threshold`) is a distinct design dimension:

- Adds a per-edge `weight` field (another state-machine axis).
- Requires a real-valued accumulator, not just counters.
- Interacts with failure policies in non-obvious ways (does a failed heavyweight edge short-circuit faster than a failed lightweight one?).
- Use cases (weighted approval, cost-weighted consensus) are real but rare; k-of-n quorum covers 90% of the RFC-007 "designed for later" ask.

Deferred to a future RFC (tentatively RFC-017-threshold-joins) once a concrete consumer surfaces. Do NOT retrofit weights into `EdgeDependencyPolicy` — add a new variant instead so existing implementations fail loud (§6.4) rather than silently misinterpret.

#### §10.4 Re-evaluating (non-one-shot) downstream eligibility under `LetRun`

Rejected. See §5 rationale. A completed execution cannot become "more eligible." Counter-updates post-`satisfied_at` are telemetry, not state transitions.

#### §10.5 A third `OnSatisfied` mode (`CompleteButDelayCancel`, etc.)

Rejected. Two modes cover the observed use cases (cancel vs. let-run). Introducing a third creates a combinatorial test matrix without a named use case. If a concrete shape surfaces later, add it then.

### §11. Implementation plan

Staged, each stage independently shippable:

1. **Stage A — `AllOf` plumbing refactor (no behavior change).**
   - Introduce `EdgeDependencyPolicy::AllOf` as the default and `set_edge_group_policy` as a no-op-defaulting op.
   - Introduce the group hash key (`ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_execution_id>`) and migrate the existing counter into it. Old in-line counter is removed.
   - `EdgeGroupSnapshot` + `describe_flow` field added; all existing snapshots report `policy = all_of`.
   - No new metrics wiring beyond `ff_edge_group_policy_total{policy=all_of}`.
   - Gate: CI green workspace-wide; RFC-007 existing flow tests unchanged semantics.

2. **Stage B — `AnyOf` + `Quorum` resolver.**
   - Extend `ff_resolve_dependency` Lua with the four-counter evaluation (§3).
   - Implement `set_edge_group_policy` Class-A op with the error cases from §6.1.
   - Wire `EdgeDependencyPolicy` through the SDK via `set_edge_group_policy` as the sole engine entry point (L.D2 disposition — `add_dependency` does NOT gain a policy parameter). Higher-level SDK wrappers (`add_quorum_group(...)`) are additive convenience above the engine surface.
   - Unit tests: all three variants, short-circuit impossibility (§3 Q3), dynamic-expansion `n` (§8.4), replay idempotence (§8.5).
   - Gate: scratch-project smoke (FlowFabric smoke harness) exercising `any_of(3)` and `quorum(2 of 3)`.

3. **Stage C — `CancelRemaining` + dispatch batching + reconciler.**
   - Introduce `cancel_execution(execution_id, reason: CancelReason)` per-exec API AND `ff_cancel_executions_batch(ids, reason)` Lua per-partition batch variant (§4.1). RFC-007 `cancel_flow` is unchanged.
   - Resolver returns `cancel_siblings_by_partition: Map<PartitionId, Vec<ExecutionId>>` on satisfied-with-cancel-remaining and persists `cancel_siblings_pending` atomically (§8.5 item 2).
   - Dispatcher cascades per-partition batched cancels and acks via `ff_ack_sibling_cancel`.
   - Introduce `ff_reconcile_pending_sibling_cancels` periodic reconciler (engine boot + ~30s interval) that iterates the per-partition `ff:pending_cancel_groups:{p:N}` index SET — never full-scans — and re-issues drained-but-unacked batches (§8.5 item 2).
   - Flow-failure-policy integration: `sibling_quorum_satisfied` / `sibling_quorum_impossible` marked as benign (Invariant Q5).
   - Metrics: `ff_edge_group_sibling_cancel_total`, `ff_edge_group_sibling_cancel_soft_cap_exceeded_total`.
   - **Gate (benchmark required before ship):** per-partition `ff_cancel_executions_batch` latency measured at N = 32, 128, 512, 1024 (§4.1) AND the concurrent-groups case (≥500 groups satisfying within a ~100ms window) to validate dispatcher-side coalescing; stage blocks until documented and under agreed threshold.
   - Gate: integration test — LLM-consensus-shaped flow (5 siblings, quorum 3, verify that on 3rd success the remaining 2 receive cancel and terminate with `cancel_reason = sibling_quorum_satisfied`).
   - Gate: crash-replay integration test — kill ff-engine mid-cancel-dispatch, restart, verify reconciler drains `cancel_siblings_pending` and Invariant Q6 holds.

4. **Stage D — `LetRun` + late-terminal observability.**
   - `OnSatisfied::LetRun` wiring (no cancels, one-shot downstream fire).
   - `ff_edge_group_let_run_late_terminal_total` metric.
   - Gate: integration test — 5-reviewer approval, quorum 2 with `LetRun`, verify that on 2nd approval the downstream fires and the remaining 3 reviewers continue to terminal without engine interference, all terminals update counters.

5. **Stage E — Observability + operator tooling.**
   - `describe_flow.edge_groups` pagination for large flows.
   - RFC-007 amendment referencing RFC-016 as the successor for lines 196-201 / 854-860.
   - Docs + examples.

Each stage lands behind a feature flag (`edge_dependency_policy_v1`) on the engine until Stage E. The flag default flips to on after Stage E CI green + smoke clean.

## References

- RFC-007 (flow DAG, dependency model, Lua resolver, cancel_flow)
- RFC-001 (execution cancel + lease-cancel signal)
- RFC-010 (Valkey partitioning, projector conventions, metric labels)
- RFC-014 (waitpoint-level multi-signal — contrast in §8.2)
- Pre-RFC: `flowfabric_use_cases_and_primitives (2).md` (UC refs: LLM consensus, human approval, racy fanout)
