# RFC-016 Round 3 — L (ergonomics)

**Verdict:** ACCEPT (confirming round-2)

Re-confirmed after round-2 revisions. Round-2 revisions added only internal mechanics (dispatch contract, index SET, coalescing); they did not change the user-facing API, worked examples, or observability surface.

Ergonomic axes:

- Worked examples (§2.1) cover the two canonical cairn patterns plus racy-fanout.
- Single engine entry point (`set_edge_group_policy`) removes dual-path ambiguity.
- `AnyOf { on_satisfied }` + `Quorum { k, on_satisfied }` are both readable.
- `OnSatisfied::{CancelRemaining, LetRun}` as a named enum correctly externalizes the design axis that distinguishes the two canonical patterns.
- Lookup primitives (`get_edge_group`, `describe_flow.edge_groups`) cover the direct + listing cases; SDK-layer name helpers explicitly out of scope.
- Metric labels (§7) are bounded; no flow/execution-id cardinality leaks.
- Forward-compat (§6.4) behavior for unknown policy variants is fail-loud.

All sections GREEN. ACCEPT.
