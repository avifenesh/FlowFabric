# RFC-016 Round 2 — M (implementation)

**Verdict:** DISSENT (narrow, overlaps K.D7)

## Round-1 disposition review

| Round-1 dissent | Resolved? |
| --- | --- |
| D1 (AllOf hash overhead) | YES — §3 storage sparseness note + §6.3 aligned; `AllOf` pays 4 fields |
| D2 (cancel batching mechanism) | YES — §4.1 + Stage C gate benchmark + soft-cap metric |
| D3 (policy_variant compactness) | PARTIAL, ACCEPTED — trade-off documented in §6.3; future-migration hook preserved via §6.4 forward-compat |
| D4 (metric label boundary) | YES — §7 label-set commitment is explicit |
| D5 (persisted pending-cancel + reconciler) | YES — §8.5 rewrite + §11 Stage C reconciler + crash-replay gate |

All round-1 implementation dissents closed.

## New findings on the revised text

### §4.1 (batching): YELLOW — see M.D6
### §8.5 (reconciler): YELLOW — overlaps K.D7, see M.D7
### §11 Stage C benchmark gate: GREEN — the N=32/128/512/1024 commitment is the right shape
### All other sections: GREEN

## Dissents

### M.D6. §4.1 "one Lua call per partition" ignores worst-case partition spread

§4.1 says: "The dispatcher issues one Lua call per partition: `ff_cancel_executions_batch(ids, reason)`." This is right for a single quorum group but wrong when many groups satisfy simultaneously. Concrete case:

Deployment has 256 Valkey hash partitions. A burst of 500 LLM-consensus flows hit their quorum within a ~100ms window (bursty upstream — e.g., a scheduled batch kickoff). Each flow has ~5 siblings spread across ~5 partitions. Naive implementation: `500 × 5 = 2500` Lua calls, each small, all slamming a few dispatcher goroutines.

The right shape is **dispatcher-side coalescing**: within a tight window (e.g., 10ms) coalesce cancel batches targeting the same partition across groups. This is an ff-engine dispatcher concern, not a Lua concern, but the RFC should commit to it so the benchmark (§11 Stage C) measures the right thing.

**Minimal change to flip ACCEPT:** Add to §4.1:

> "Dispatcher-side coalescing: the ff-engine cancel dispatcher SHOULD coalesce `cancel_siblings_by_partition` entries across groups within a bounded window (implementation guidance: ~10ms, tunable) so that N concurrent satisfied groups targeting the same partition result in O(partitions-touched) Lua calls, not O(groups × partitions-per-group). This is a dispatcher concern, not a Lua surface change. Stage C benchmark (§11) MUST exercise the 500-concurrent-groups case, not only the single-group-fan-out case."

### M.D7. §8.5 reconciler scan cost at scale (same as K.D7)

Concurred with K.D7. The revised §8.5 says the reconciler scans for groups with `group_state = satisfied AND cancel_siblings_pending != []`. At millions of groups, a periodic full scan is a hot-path anti-pattern — and it directly contradicts the project-wide stance ("SCAN → SETs" per the Cairn-blocking infra batch).

**Minimal change to flip ACCEPT:** §8.5 item 2 adds the per-partition index SET:

> "`ff:pending_cancel_groups:{p:N}` — SET of `downstream_execution_id` values whose edge group has `cancel_siblings_pending` non-empty on partition `p:N`. Populated atomically by `ff_resolve_dependency` when flipping `satisfied` with `CancelRemaining` and pending > 0. Drained by `ff_ack_sibling_cancel` when the pending list empties. The reconciler iterates this SET per partition — NO full-hash-scan. On engine boot, the reconciler processes every partition's SET. Periodic reconciliation (~30s) re-processes; normal dispatch drains in the common case, so periodic runs see an empty or near-empty SET."

This aligns with the broader project stance (from Cairn-blocking batch A: "SCAN → SETs") and makes the reconciler scale-safe.

## Summary

All 5 round-1 implementation blockers closed cleanly. Two narrow residual dissents on the new content — one on dispatcher coalescing (M.D6), one on reconciler index SET (M.D7, shared with K.D7). Both are specification additions, no design change.

Flip to ACCEPT if both §4.1 and §8.5 are tightened as above.
