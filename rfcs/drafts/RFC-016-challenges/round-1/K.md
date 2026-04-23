# RFC-016 Round 1 — K (correctness)

**Verdict:** DISSENT

## Per-section

- §2 (policy shape): GREEN
- §3 (edge-group state machine): YELLOW — Q3 short-circuit + LetRun interaction ambiguous; see D1
- §4 (CancelRemaining semantics): RED — replay race between atomic "satisfied" flip and dispatcher-side sibling-cancel emission is underspecified; see D2
- §5 (LetRun semantics): YELLOW — "late-arriving success under impossible" vs "impossible short-circuit under LetRun" overlap; see D3
- §6 (API + wire): YELLOW — `set_edge_group_policy` ordering rules vs. `add_dependency` are not mechanically enforceable as written; see D4
- §7 (observability): GREEN
- §8.4 (dynamic expansion): RED — `k <= n` invariant is evaluable only at resolve time, and the RFC admits edges may still be staging when an upstream terminal lands; the "wait" behavior silently masks a `k > n` bug class; see D5
- §8.5 (replay): YELLOW — claim that the resolver "recomputes the list from live sibling lifecycle state" collides with cross-partition visibility; see D2
- §10 / §11: GREEN

## Dissents (each blocks ACCEPT until resolved in the RFC text)

### D1. §3 Q3 short-circuit under `LetRun` is contradictory

§3 step 5 says on `impossible`, "transition downstream to `terminal_outcome = skipped` ... and cascade-resolve." Q3 says under `LetRun` the short-circuit "skips the downstream but leaves siblings running." Combined with §5 paragraph 2 ("once impossible, always impossible"), this is consistent — but §5 paragraph 1 ("no cancels are issued" under LetRun) refers only to the **satisfied** branch. The `impossible` branch under `LetRun` issuing no cancels is stated only in Q3, not §5, and the cross-reference is easy to miss.

**Minimal change to flip ACCEPT:** In §5, add an explicit sub-paragraph:

> "Under `LetRun`, when `group_state` flips to `impossible` (Q3 short-circuit), the downstream is marked `skipped` and skip-propagation runs as today, but still-running siblings are NOT cancelled — consistent with LetRun's one-shot, no-rescind contract. Late sibling terminals continue to update counters for observability."

### D2. §4 + §8.5 sibling-cancel recomputation is not replay-safe as specified

§8.5 item 2 says: "the next resolver on the same group sees `group_state = satisfied` and re-emits the remaining non-terminal siblings in `cancel_siblings` (the resolver recomputes the list from live sibling lifecycle state, not a persisted queue)."

Problem: sibling executions live on **their own** partitions (RFC-007 §Partitioning — execution partition is determined by execution id, not downstream id). `ff_resolve_dependency` runs atomically on the downstream's partition. It can read the group hash there, but it **cannot** read sibling execution lifecycle state atomically — that requires a cross-partition read, which Lua cannot do.

Two consequences:
1. Recomputing `cancel_siblings` on replay requires the resolver to emit the original sibling set (from the edge-group's edge list, which IS co-located on the downstream's partition — edges are keyed by downstream), and let the dispatcher no-op against already-terminal siblings. That is workable, but §8.5 wording ("recomputes from live sibling lifecycle state") is wrong as written.
2. There is no "next resolver on the same group" in the common case after `satisfied` — if no new upstream terminals land, no resolver runs again. Recovery must therefore be driven by engine boot / reconciliation, not by the next terminal.

**Minimal change to flip ACCEPT:**
1. §8.5 item 2 rewrite:
   > "Sibling cancel dispatch is a separate step performed by ff-engine after Lua returns. The resolver persists `cancel_siblings_pending: [execution_id, ...]` into the group hash within the same atomic transaction as the `satisfied` flip. The dispatcher drains this field and issues `cancel_execution` per sibling; each successful dispatch removes the id from the field (atomic Lua `ff_ack_sibling_cancel`). On engine crash mid-dispatch, recovery (engine boot or periodic reconciler) reads groups with non-empty `cancel_siblings_pending` and re-issues. Duplicate cancels against already-terminal siblings are no-ops (§4 race handling)."
2. Remove the "recomputes from live sibling lifecycle state" sentence — it is impossible across partitions.
3. Add Invariant Q6: "`cancel_siblings_pending` is empty iff every non-terminal sibling at `satisfied_at` has received exactly one `cancel_execution` dispatch."

### D3. §5 "late-arriving success under impossible" double-counts with §3 step 1

§3 step 1 says counters always update on upstream terminal, even after `group_state != pending`. §5 restates that for LetRun. But the RFC never addresses: under `AllOf`, once `group_state = impossible` from the first non-success terminal, do subsequent success terminals still increment `succeeded`? Today's behavior (RFC-007 `all_required`) doesn't track `succeeded` separately — impossibility is fatal and done. The new four-counter model says "yes, keep counting for observability" — but that means `AllOf` group snapshots will show `succeeded=3, failed=1` for a group that was marked impossible on the failed terminal, which is weird to explain to operators.

**Minimal change to flip ACCEPT:** In §3 step 2, clarify: "Counters continue to update under all policies (including `AllOf` post-impossible) for observability. Operator-facing docs must note that `succeeded + failed + skipped` may reach `n` even after `group_state = impossible` — impossibility is one-shot, counters are not."

### D4. §6.1 ordering rule ("before any inbound edge transitions to active") is not mechanically enforceable

The rule says `set_edge_group_policy` "Must be called BEFORE any inbound edge to `downstream_execution_id` transitions to `active`." But edges transition to active when their upstream terminates — and the RFC allows `add_dependency` to stage edges before any policy is set (with implicit `AllOf` default).

Concrete failure shape: user calls `add_dependency(A→D)`, A is already terminal-success, so the edge resolves immediately and the group initializes implicitly as `AllOf`. User then calls `set_edge_group_policy(D, Quorum{k:2, ...})` → error `group_policy_fixed_after_activation`. User is surprised because they never saw a "the group is already activated" signal.

**Minimal change to flip ACCEPT:** Either:
- (a) Require `set_edge_group_policy` to be called before the FIRST `add_dependency` targeting `downstream_execution_id` (stricter, SDK-enforceable), OR
- (b) Allow policy change up until the first resolver RUN (not first edge staging), and document that calling `add_dependency` on an already-terminal upstream will run the resolver synchronously and thus activate the group.

Pick one explicitly in §6.1. Current text straddles both.

### D5. §8.4 `Quorum{k}` with dynamic expansion and `k > n` transient state silently waits

§8.4 says: "if the group still has not reached `k` edges when an upstream resolves, the resolver simply waits (same as today's `AllOf` waiting for more edges to stage)."

Problem: `AllOf` "waits" for edges because the check is `succeeded == n` and `n` keeps growing — this is well-defined. For `Quorum{k}`, the check is `succeeded >= k`. If only 1 edge has been staged and it succeeds with `k=3`, the resolver sets counters and returns `pending` — correct. But the user may have intended `k=3 of 3`, the edge-adder crashed before staging edges 2 and 3, and the group sits `pending` forever with no visibility. Under `AllOf`, the same scenario sits forever too — but `AllOf` doesn't carry a user-chosen `k` that implicitly asserts "I expect >= k edges."

This is a correctness issue only if the RFC claims `Quorum{k,n}` semantics are enforced. §6.1 says `k <= n` is validated at resolve time, so the transient `k > n` state is tolerated. Make this tolerance an explicit commitment.

**Minimal change to flip ACCEPT:** In §8.4, add:
> "A group where `k > current_n` at resolve time is treated as `pending` (waiting for more edges), not as an error. Flows that want strict `k-of-N` declaration with N known up-front must use static edge staging (no dynamic expansion for this group) OR call `set_edge_group_policy` after all edges are staged — at which point `k > n` raises `invalid_policy`."

Also add an observability hook: `ff_edge_group_evaluation_total{outcome=pending, reason=k_exceeds_n}` so operators can detect stuck-waiting groups.

## Summary

RFC is close but has five concrete text defects that each independently undermine correctness under replay, dynamic expansion, or operator mental-model. D2 is the highest-severity — cross-partition sibling visibility was asserted where it doesn't hold. The other four are clarifying-and-binding text additions.
