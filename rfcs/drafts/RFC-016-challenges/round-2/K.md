# RFC-016 Round 2 — K (correctness)

**Verdict:** DISSENT (narrow)

## Round-1 disposition review

| Round-1 dissent | Resolved in round-1 revisions? |
| --- | --- |
| D1 (LetRun + impossible) | YES — §5 new paragraph is clear |
| D2 (persisted cancel_siblings_pending) | YES — §8.5 rewrite + Q6 + reconciler; correct across partitions |
| D3 (AllOf post-impossible counters) | YES — §3 step 2 explicit |
| D4 (set_edge_group_policy ordering) | YES — option (a) picked, mechanical rule |
| D5 (dynamic expansion k > n) | YES — §8.4 + metric reason |

All round-1 correctness dissents are closed.

## New findings on the revised text

### §3 (state machine): GREEN
### §4.1 (batching): YELLOW — see D6
### §8.5 (replay): YELLOW — see D7
### All other sections: GREEN

## Dissents

### D6. §4.1 batch atomicity vs. partial-batch failure is unspecified

`ff_cancel_executions_batch(ids, reason)` runs on a single partition (all `ids` share that partition by construction). Inside the Lua, each id flips a different execution's lifecycle record. Two failure shapes are not covered:

1. **Partial success inside Lua:** one id in the batch is already `terminal` (race with the sibling worker finishing first) — today's §4 race-handling says this is a no-op and logs `cancel_sibling_ignored_terminal`. Fine for correctness. But the batch Lua must still return a per-id disposition list so the dispatcher can ack selectively via `ff_ack_sibling_cancel`. If it returns a bulk success, the dispatcher has no way to learn which ids were actually flipped vs. no-op'd.
2. **Lua error mid-batch (e.g., OOM, cluster slot loss):** Valkey Lua is atomic — the whole script fails or succeeds. But the dispatcher then has to decide: treat the whole batch as "not dispatched, retain in `cancel_siblings_pending`" and re-issue on next reconciler tick. That is correct, but it's not written down.

**Minimal change to flip ACCEPT:** In §4.1, add a dispatch contract paragraph:

> "`ff_cancel_executions_batch(ids, reason)` returns a per-id disposition list: `[(execution_id, cancelled | already_terminal | not_found), ...]`. The dispatcher acks `cancelled` AND `already_terminal` entries (both are successful resolutions — the cancel signal did or did not need to be delivered; the group's obligation is met) via `ff_ack_sibling_cancel`. `not_found` entries are logged and NOT acked — they remain in `cancel_siblings_pending` for reconciler retry, in case the sibling execution record arrives late via projector lag. A Lua error aborts the whole batch; no acks are issued; the reconciler re-issues on its next tick."

This closes the ambiguity without changing the design.

### D7. §8.5 reconciler sweep cost at deployment scale

Round-1 revision §8.5 says the reconciler scans "groups with `group_state = satisfied AND cancel_siblings_pending != []`." At deployment scale (100k+ flows, millions of edge groups across partitions), this is a full scan on every ~30s tick. Implemented naively it is a cross-partition SCAN + HGET per group — load-bearing on the control path.

Two mitigation approaches (pick one):
- (a) Per-partition index SET: `ff:pending_cancel_groups:{p:N}` containing downstream_execution_ids with non-empty `cancel_siblings_pending`. Populated by `ff_resolve_dependency` on `satisfied` flip; drained by `ff_ack_sibling_cancel` when the pending set empties. Reconciler iterates the index SET, not a full scan.
- (b) Reconciler runs only on engine boot (one-shot), and the normal dispatch path is trusted between. Crashes between dispatch and ack are the only recovery window.

Option (a) is correct-by-design for periodic reconciler; (b) narrows the recovery contract to "crash-then-restart" and avoids periodic scans entirely but makes the contract weaker (no recovery from a soft hang of the dispatcher goroutine, only from a full restart).

**Minimal change to flip ACCEPT:** §8.5 item 2 pick one:
- Either add the index SET (preferred — matches the broader project stance on replacing SCAN with SETs per project memory), OR
- Restrict the reconciler to engine-boot-only and remove the "every 30s" note.

Either is fine; just commit explicitly. Current text hand-waves the scan cost.

## Summary

All 5 round-1 dissents closed. Two new narrow dissents on the **new** content (§4.1, §8.5 reconciler). Both are textual additions, no design change.
