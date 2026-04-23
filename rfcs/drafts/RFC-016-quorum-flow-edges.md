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
| `failed` | u32 | Upstream terminal = failed (error, timeout, cancelled, attempt-exhausted). |
| `skipped` | u32 | Upstream terminal = skipped (skip-propagation from further upstream). |
| `running` | u32 | Derived: `n - (succeeded + failed + skipped)`. Not stored; computed. |
| `group_state` | enum | `pending` → `satisfied` \| `impossible` \| `cancelled`. One-shot. |
| `satisfied_at` | unix ms, nullable | When quorum was first met (for `LetRun` observability). |

**Transitions.** Evaluated atomically on the downstream's partition inside `ff_resolve_dependency` (RFC-007 line 398, 623) each time an upstream terminal lands:

1. On upstream terminal: increment the matching counter (`succeeded` / `failed` / `skipped`).
2. If `group_state != pending`, stop (one-shot; already fired). Counters still update for observability.
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

### §5. `LetRun` semantics

Under `OnSatisfied::LetRun`, when `group_state` flips to `satisfied`, no cancels are issued. Still-running siblings continue to terminal. Their terminal outcomes update the counters for observability (§3 step 1) but, because of Invariant Q1, never retrigger downstream eligibility — the downstream fires exactly once.

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

- Must be called BEFORE any inbound edge to `downstream_execution_id` transitions to `active` (i.e., before any upstream resolves). Typically called immediately after `create_child_execution` and before the first `add_dependency`.
- Default if never called: `EdgeDependencyPolicy::AllOf`. Existing flows created pre-RFC-016 are implicitly `AllOf` — full backward compatibility.
- For `Quorum { k }`, the final `k` must satisfy `1 <= k <= n` where `n` is `edge_count` on the downstream group at `group_state` evaluation time. If the flow uses dynamic expansion (`dynamic_expansion_enabled`), `n` is measured each time the resolver runs; if the group still has not reached `k` edges when an upstream resolves, the resolver simply waits (same as today's `AllOf` waiting for more edges to stage).
- Errors: `invalid_policy` (k < 1), `policy_already_set` (attempt to change after active), `group_policy_fixed_after_activation` (any edge in group already resolved).

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

`EdgeSnapshot` itself is unchanged. Per-edge identity, upstream/downstream pointer, and `edge_state` (pending/satisfied/impossible/cancelled) all keep their RFC-007 meanings.

#### §6.3 Wire format (Valkey)

RFC-007 line 453 defines edge keys. RFC-016 adds:

- `ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_execution_id>` — hash with fields `policy_variant`, `k` (only for `Quorum`), `on_satisfied`, `n`, `succeeded`, `failed`, `skipped`, `group_state`, `satisfied_at`.
- `policy_variant` values: `all_of` | `any_of` | `quorum` (string, forward-compatible with future additions).

`ff_resolve_dependency` reads the group hash, increments the counter, evaluates satisfaction, and returns an action record `{ eligibility: satisfied|impossible|pending, cancel_siblings: [...] }`. The dispatcher side is unchanged except it honors the new `cancel_siblings` list alongside the existing `child_skipped` cascade.

#### §6.4 Forward-compat

`policy_variant` is a string, not a bit flag. If a future RFC adds `Threshold` (see §10.3), existing engines that see an unknown variant MUST return `unsupported_policy_variant` and refuse to resolve the group — fail loud, never silently misinterpret. Projection layers / SDK older than the engine MUST surface the raw variant string rather than crash on enum-decode.

### §7. Observability

New metrics (Prometheus labels follow RFC-010 conventions):

- `ff_edge_group_policy_total{policy}` — gauge of active groups by policy (sampled from projector).
- `ff_edge_group_evaluation_total{policy, outcome}` — counter of resolver evaluations. `outcome ∈ { pending, satisfied, impossible }`.
- `ff_edge_group_sibling_cancel_total{reason}` — counter of sibling cancels issued. `reason ∈ { sibling_quorum_satisfied, sibling_quorum_impossible }`.
- `ff_edge_group_let_run_late_terminal_total{terminal}` — counter of terminals arriving after `satisfied` under `LetRun`. `terminal ∈ { success, failed, skipped, cancelled }`. Useful to detect budget waste under mis-tuned `LetRun`.

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

#### §8.5 Replay (RFC-001 + RFC-007)

If the engine crashes between "resolver decides satisfied" and "downstream eligibility flip + sibling cancel dispatch," replay must be idempotent. Guarantees:

1. `group_state`, `succeeded/failed/skipped`, and `satisfied_at` are stored in the group hash on the downstream's `{p:N}` partition within the same atomic Lua `ff_resolve_dependency` call that reads them. Counter increment + satisfaction decision + downstream-eligibility flip is one atomic transaction.
2. Sibling cancel dispatch is a **separate** step performed by ff-engine after Lua returns. On crash mid-dispatch, the next resolver on the same group sees `group_state = satisfied` and re-emits the remaining non-terminal siblings in `cancel_siblings` (the resolver recomputes the list from live sibling lifecycle state, not a persisted queue). Duplicate cancels against already-terminal siblings are no-ops (§4 race handling).
3. Downstream eligibility flip is idempotent: transitioning an execution from `eligible_now` to `eligible_now` is a no-op in RFC-001.
4. `satisfied_at` is set once (conditional Lua update: `HSETNX` equivalent). Replay does not overwrite it.

### §9. Open questions

1. **`AnyOf { on_satisfied }` with `LetRun`: is this a real use case, or only `AnyOf { CancelRemaining }`?** Racy-mirror use cases want cancel. Human approval with `k=1` is unusual ("any reviewer approves, skip the rest") but plausible. Leaning: keep `LetRun` on `AnyOf` for symmetry; revisit if no consumer hits it within 6 months.
2. **Should `sibling_quorum_impossible` also count as a cancel under `LetRun`?** Today §3 says no — `LetRun` means "don't cancel, ever." But if the group is impossible, siblings will only produce waste. Counter-argument: the user picked `LetRun` knowing some siblings are "for the record." Lean: keep LetRun semantics pure — never cancel under LetRun, even on impossible. Revisit with consumer input.
3. **Cross-partition cost of `ff_edge_group_sibling_cancel_total` at high fanout.** For `quorum(1 of 1000)` (hypothetical stress shape), the first success triggers 999 cross-partition cancels. This is similar to the RFC-007 §Known v1 limitation large-fan-out burst and likely has the same mitigation (partition-batch + stagger). Needs a benchmark before declaring any `n` cap.

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
   - Wire `EdgeDependencyPolicy` through the SDK (`add_dependency` gains an optional group-policy parameter as sugar; `set_edge_group_policy` is the primary op).
   - Unit tests: all three variants, short-circuit impossibility (§3 Q3), dynamic-expansion `n` (§8.4), replay idempotence (§8.5).
   - Gate: scratch-project smoke (FlowFabric smoke harness) exercising `any_of(3)` and `quorum(2 of 3)`.

3. **Stage C — `CancelRemaining`.**
   - Introduce `cancel_execution(execution_id, reason: CancelReason)` per-exec API. This is a new engine surface; RFC-007 `cancel_flow` is unchanged.
   - Resolver returns `cancel_siblings: [execution_id, ...]` on satisfied-with-cancel-remaining.
   - Dispatcher cascades per-exec cancels cross-partition.
   - Flow-failure-policy integration: `sibling_quorum_satisfied` / `sibling_quorum_impossible` marked as benign (Invariant Q5).
   - Metrics: `ff_edge_group_sibling_cancel_total`.
   - Gate: integration test — LLM-consensus-shaped flow (5 siblings, quorum 3, verify that on 3rd success the remaining 2 receive cancel and terminate with `cancel_reason = sibling_quorum_satisfied`).

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
