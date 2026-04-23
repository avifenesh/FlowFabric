# RFC-016 Round 3 — M (implementation)

**Verdict:** ACCEPT

## Round-2 disposition review

| Round-2 dissent | Resolved? |
| --- | --- |
| M.D6 dispatcher-side coalescing across groups | YES — §4.1 coalescing paragraph + §11 Stage C benchmark extended to ≥500 concurrent groups |
| M.D7 reconciler scan cost (index SET) | YES — §6.3 defines `ff:pending_cancel_groups:{p:N}`; §8.5 item 2 commits the resolver + ack mutators; §11 Stage C reconciler description aligned |

Both residual implementation dissents closed.

## Per-section sweep of the revised RFC

- §3 storage sparseness + §6.3 sparse write contract: GREEN — `AllOf` hot-path unchanged, new policies pay their own cost
- §4.1 batching + coalescing + dispatch contract: GREEN — atomicity, bounded window, per-partition Lua call shape all specified
- §6.3 wire format incl. index SET: GREEN — keys align with project conventions; `policy_variant` string retained with explicit trade note
- §7 metrics labels (incl. anti-goal on flow_id/execution_id): GREEN
- §8.5 replay with persisted pending + index SET + reconciler: GREEN — no hand-waved recompute-from-state paths
- §11 staging: GREEN — Stage C gate benchmark is concrete (N = 32/128/512/1024 single-group + ≥500 concurrent) and crash-replay test is explicit

Residual open items in §9 (LetRun on AnyOf; cancel-on-impossible-under-LetRun; high-N cap) are genuine future-consumer questions, not specification gaps.

No residual implementation concerns. ACCEPT.
