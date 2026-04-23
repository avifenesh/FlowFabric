# RFC-016 Round 1 — M (implementation)

**Verdict:** DISSENT

## Per-section

- §3 state machine: YELLOW — four counters plus `group_state` plus `satisfied_at` on every downstream is non-trivial storage growth at high flow-graph density; see D1
- §4 CancelRemaining: RED — cross-partition sibling-cancel at high fanout is the dominant cost and §9 item 3 defers it to "needs a benchmark" without even an advisory cap; see D2
- §6.3 wire format: YELLOW — storing `policy_variant` as a string on every edge-group hash (one per downstream, up to millions of downstreams per deployment) is wasteful; see D3
- §7 metrics: YELLOW — `ff_edge_group_evaluation_total{policy, outcome}` + `ff_edge_group_let_run_late_terminal_total{terminal}` are high-cardinality at scale; see D4
- §8.5 replay: RED — cancel-dispatch is described as "after Lua returns" with no persisted pending-set, same issue as K.D2 but viewed from the implementation side; see D5
- §11 staging: GREEN — A→B→C→D→E staging is sensible

## Dissents

### D1. Every `AllOf` downstream now carries group-hash overhead

RFC-007's `all_required` path uses a single counter per downstream. Stage A migrates this to the full group hash (`policy_variant`, `n`, `succeeded`, `failed`, `skipped`, `group_state`, `satisfied_at` — 7 fields) **even for `AllOf`**. On a realistic flow with 10k downstream nodes, that is ~70k Valkey hash fields where today there are ~10k counter fields. Roughly 7x overhead on edge-state for the 99% case that never uses quorum.

Options:
- (a) Keep the hash only for non-`AllOf` groups; `AllOf` keeps its existing counter and a one-field policy marker. Resolver branches on presence of the group hash.
- (b) Store only non-default fields; omit `satisfied_at`, `failed`, `skipped` for `AllOf` since `AllOf` short-circuits on first non-success and never needs them.
- (c) Accept the overhead; document the 7x edge-state cost and move on.

**Minimal change to flip ACCEPT:** §3 + §6.3 must explicitly state which option applies. Leaning (b) — `AllOf` writes only `policy_variant=all_of`, `n`, `succeeded`, `group_state`; `AnyOf`/`Quorum` writes the full set. This keeps the hot-path storage close to today's footprint while the new policies pay their own cost. Add to §3:

> "Counter fields are sparse: `AllOf` groups maintain only `policy_variant`, `n`, `succeeded`, `group_state`. `failed`/`skipped`/`satisfied_at` are written only by `AnyOf`/`Quorum` policies. Resolvers read whichever fields their policy requires."

### D2. §4 cross-partition cancel at high fanout has no advisory cap or batching plan

§9 item 3 says "needs a benchmark before declaring any `n` cap." That is fine for the final number, but the RFC should commit to a **mechanism** so the benchmark measures the right thing. Today's RFC-007 child-skip cascade (line 885) is one cancel per terminal, amortized across terminals that arrive over time. Quorum-triggered cancels are bursty: `quorum(1 of N)` produces N-1 cancels **in the same tick** as the first success.

Concrete concerns:
1. The dispatcher emits `cancel_execution` as individual Valkey commands. At N=1000, that is 999 round-trips from ff-engine to Valkey in ~O(ms) window, followed by per-partition Lua invocations. This will saturate the single engine dispatcher loop and stall unrelated flows.
2. Cross-cluster partition-hops: each cancel targets a sibling's `{p:N}` partition, potentially spread across all partitions. MOVED responses during cluster rebalance multiply this.
3. No per-flow cap means a malicious or misconfigured flow can emit arbitrary cancel storms.

**Minimal change to flip ACCEPT:** Add §4.1 "Cancel dispatch batching":
> "Sibling cancels are dispatched in partition-grouped batches. The resolver returns `cancel_siblings_by_partition: Map<PartitionId, Vec<ExecutionId>>` and the dispatcher issues one Lua call per partition (`ff_cancel_executions_batch(ids, reason)`). An advisory soft cap of 128 siblings per group is emitted as a warning metric `ff_edge_group_sibling_cancel_soft_cap_exceeded_total` when exceeded; no hard cap in v1. Benchmark (§9 item 3) measures per-partition batch latency at N=32, 128, 512, 1024."

Also move §9 item 3 from "open question" to "benchmark requirement for Stage C gate" in §11.

### D3. §6.3 `policy_variant` as a string is wasteful at scale

Storing the literal string `"all_of"`/`"any_of"`/`"quorum"` on every edge-group hash is ~7-8 bytes per group where a single-byte enum discriminant suffices. At 10M groups per large deployment that is ~80MB of duplicated strings. §6.4 forward-compat argument ("strings are forward-compatible with future additions") is real but solvable with a registered-enum table and a single-byte tag (or `0x01/0x02/0x03` strings).

**Minimal change to flip ACCEPT:** §6.3 revise:
> "`policy_variant` is stored as a compact numeric code (u8) per group: `1=all_of`, `2=any_of`, `3=quorum`. Unknown codes MUST be surfaced raw to upstream decoders (§6.4) rather than silently mapped. The SDK/projector translates to the stable string form for external APIs and metrics labels."

If the author prefers strings for debuggability, that's a legitimate trade — document it. Current text doesn't acknowledge the trade.

### D4. §7 metric cardinality

- `ff_edge_group_evaluation_total{policy, outcome}`: 3 policies × 3 outcomes = 9 series. Fine.
- `ff_edge_group_sibling_cancel_total{reason}`: 2 reasons. Fine.
- `ff_edge_group_let_run_late_terminal_total{terminal}`: 4 terminals. Fine.
- `ff_edge_group_policy_total{policy}`: 3 series. Fine.

All fine. Withdrawing cardinality concern — but: §7 should commit that no labels will be added in v1 that include `flow_id`, `execution_id`, or user-chosen names. Explicit anti-goal.

**Minimal change to flip ACCEPT:** Add to §7:
> "Metric labels are bounded to policy/outcome/reason/terminal. `flow_id`, `execution_id`, and user-assigned names are NEVER labels in v1. Per-flow quorum diagnostics live in `describe_flow.edge_groups` snapshots, not metrics."

### D5. §8.5 replay: persist the pending-cancel set

Mirrors K.D2 from the implementation side. The RFC's claim that the resolver "recomputes the list from live sibling lifecycle state" on replay is not implementable — Lua on the downstream's partition cannot read sibling execution state atomically (siblings live on their own partitions). Even outside Lua, the dispatcher's view of sibling state is stale under contention.

**Minimal change to flip ACCEPT:** Same fix as K.D2 — persist `cancel_siblings_pending: [execution_id, ...]` in the group hash within the atomic `satisfied` transaction, drain on dispatch, recover on boot. §8.5 item 2 rewrite is the same as K.D2.

Additional implementation note the RFC should add: the dispatcher needs a new reconciler job (`ff_reconcile_pending_sibling_cancels`) that scans groups with `group_state=satisfied AND cancel_siblings_pending != []` on engine boot and periodically (e.g., every 30s). Add to §11 Stage C.

## Summary

Three implementation blockers (D1 storage overhead for default `AllOf` path, D2 fanout cancel mechanism, D5 replay mechanics) and two specification nits (D3 wire compactness, D4 metric label boundary). D2 and D5 are hardest — the RFC underspecifies cost control and recovery. D1 is a hot-path footgun that should be called out explicitly rather than discovered in Stage A.
