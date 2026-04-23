# RFC-016 Round 2 — L (ergonomics)

**Verdict:** ACCEPT

## Round-1 disposition review

| Round-1 dissent | Resolved? |
| --- | --- |
| D1 (worked examples) | YES — §2.1 covers both canonical patterns + racy-fanout |
| D2 (dual entry-point API) | YES — §6.1 + §11 Stage B consistently state `set_edge_group_policy` is sole engine entry |
| D3 (name-based lookup) | YES — §6.2 commits to direct-by-uuid + SDK-layer name helpers |

## Per-section (post-revision)

- §1 Summary: GREEN
- §2 / §2.1 / §2 shape: GREEN — the `Quorum(3-of-5, CancelRemaining)` + `Quorum(2-of-5, LetRun)` worked examples read cleanly; `AnyOf { on_satisfied }` as a named convenience pulls its weight next to `Quorum { k, on_satisfied }`.
- §6.1 API: GREEN — single entry point, mechanical ordering rule, SDK wrappers additive.
- §6.2 snapshot: GREEN — `EdgeGroupSnapshot` is the right shape for the two canonical UIs (vote tally, approval progress).
- §7 observability: GREEN — `reason` label on pending outcomes makes stuck-waiting `Quorum` groups visible to operators, which is the right dial.
- §8.2 RFC-014 contrast: GREEN — the `describe_flow` docs callout is the right place for operator guidance.
- §11 stages: GREEN — worked examples in §2.1 + staged gates give a clear SDK implementation roadmap.

## Notes (non-blocking, author's discretion)

1. The `add_quorum_group(flow_id, downstream, upstreams, policy)` helper mentioned in §6.1 + §11 Stage B is a strictly additive SDK concern. The RFC correctly doesn't specify its signature — leave it to the SDK PR. No change requested.

2. The §2.1 LLM-consensus example uses `k: 3` on 5 voters. Nit: a real consumer would probably want `k: ceil(n/2) + 1 = 3` explicit for clarity, but the example is fine as-is.

3. §6.2 caps `edge_groups` pagination at 256. That threshold isn't justified and isn't load-bearing for ergonomics — fine to leave. If consumer telemetry shows 256 is wrong, tune in a follow-up.

## Summary

All three ergonomics concerns from round 1 are resolved by the revisions. The API reads cleanly for both canonical cairn patterns. ACCEPT.
