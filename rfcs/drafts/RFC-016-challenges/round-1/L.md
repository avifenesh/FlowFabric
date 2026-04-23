# RFC-016 Round 1 — L (ergonomics)

**Verdict:** DISSENT

## Per-section

- §1 Summary: GREEN
- §2 shape: YELLOW — `AnyOf { on_satisfied }` + `Quorum { k, on_satisfied }` is readable, but the cairn LLM-consensus example would make the API choice concrete; see D1
- §6.1 API: RED — two entry points (`set_edge_group_policy` + optional sugar on `add_dependency`) with an ordering constraint is a footgun for the LLM-consensus pattern where the policy is the primary intent; see D2
- §6.2 snapshot: YELLOW — `get_edge_group` keyed by `(flow_id, downstream_execution_id)` requires callers to know the synthetic downstream id; human-approval observability usually asks "show me reviewer progress for approval-step X"; see D3
- §7 observability: GREEN
- §8.2 RFC-014 contrast: GREEN (framing is good; the table sells the separation)
- §11 Implementation stages: GREEN

## Dissents

### D1. No worked example in the RFC for the two canonical patterns

The Motivation section names LLM-consensus and human-approval but never shows flow-definition pseudocode for either. An ergonomics RFC that doesn't show the happy-path call site is hard to evaluate — every reader re-derives it differently.

The cairn LLM-consensus pattern (`Quorum(3-of-5)` voting with `CancelRemaining`) reads cleanly in pseudocode something like:

```rust
let voters: Vec<ExecutionId> = (0..5).map(|_| create_child_execution(...)).collect();
let decision = create_child_execution("decide", ...);
set_edge_group_policy(flow_id, decision, Quorum { k: 3, on_satisfied: CancelRemaining });
for v in &voters { add_dependency(flow_id, v, decision, EdgeSpec::default()); }
```

The human-approval pattern (`Quorum(2-of-5)` with `LetRun`):

```rust
let reviewers: Vec<ExecutionId> = ...;
let approved = create_child_execution("approved", ...);
set_edge_group_policy(flow_id, approved, Quorum { k: 2, on_satisfied: LetRun });
for r in &reviewers { add_dependency(flow_id, r, approved, EdgeSpec::default()); }
```

**Minimal change to flip ACCEPT:** Add §2.1 "Worked examples" with exactly these two snippets (or author's preferred flavor) BEFORE §3. Without it, the downstream stages lack a grounded referent.

### D2. Two-entry-point API (`set_edge_group_policy` + `add_dependency` sugar) is a footgun

§6.1 approach B is correct (per-group, not per-edge) but §11 Stage B says "`add_dependency` gains an optional group-policy parameter as sugar; `set_edge_group_policy` is the primary op." Sugar + primary op with ordering constraints ("before first edge activates") will produce:

1. Users who pass policy to `add_dependency` on edge #2 after edge #1 is already staged and quietly overriding/conflicting.
2. Users who call `set_edge_group_policy` and then pass a *different* policy on `add_dependency` and hit `policy_already_set`.
3. Library wrappers that do both defensively.

**Minimal change to flip ACCEPT:** Pick ONE:
- (a) Drop the `add_dependency` sugar in v1. `set_edge_group_policy` is the only way to set policy. `add_dependency` has no policy knob. SDK can offer higher-level helpers (e.g., `add_quorum_group(flow_id, downstream, upstreams, policy)`) that wrap both.
- (b) Keep the sugar but make it a **builder** that refuses to commit until policy is set (e.g., `begin_edge_group(downstream, policy).add_edge(u1).add_edge(u2).commit()`). No ordering footgun because commit is atomic.

Recommend (a) for v1 — simplest API, easiest to reason about, and the SDK helper is additive.

Also: strike the Stage B line "`add_dependency` gains an optional group-policy parameter as sugar." It conflicts with §6.1's approach-B choice.

### D3. `get_edge_group` lookup keyed only by downstream id is hard to use

In the human-approval pattern the UI wants to show "approval block for deploy step X: 2/5 approved." The deploy step has a human-friendly name ("deploy-prod") but `downstream_execution_id` is a Uuid. `describe_flow` returns `edge_groups: Vec<EdgeGroupSnapshot>` which lets callers find by scanning, but the direct lookup requires holding the Uuid.

This is an SDK ergonomics nit, not an engine concern. But if we ship `get_edge_group(flow_id, downstream_execution_id)` in v1 without a name-based lookup, consumers will build it themselves inconsistently.

**Minimal change to flip ACCEPT:** Either:
- (a) Add `get_edge_group_by_name(flow_id, downstream_name)` to §6.2 as a sibling API, OR
- (b) Note in §6.2 that `describe_flow` is the canonical listing and `get_edge_group` is the direct-lookup primitive; name-based helpers are SDK concerns out of this RFC.

Either is fine; just pick and commit. Current text leaves it ambiguous.

## Summary

The RFC is ergonomically solid on the big axes: `AnyOf`/`Quorum` split is right, `OnSatisfied` as a named enum (not a bool) is right, `LetRun` vs `CancelRemaining` covers the observed patterns. Three concrete text defects prevent ACCEPT: missing worked examples (D1), dual-entry-point API (D2), and underspecified lookup ergonomics (D3). All three are text-only fixes.
