# RFC-016 — Team-debate outcome: ACCEPTED

**Unanimous ACCEPT reached after 3 rounds.** K (correctness), L (ergonomics), M (implementation) all accept the revised RFC.

## Round summary

### Round 1 — 3 dissents each from K and M, 3 from L (11 total)

- **K (correctness, DISSENT):** LetRun-under-impossible ambiguity; cross-partition sibling recompute on replay is impossible; AllOf post-impossible counter semantics; ordering rule not enforceable; dynamic-expansion `k > n` silent-wait.
- **L (ergonomics, DISSENT):** no worked examples; dual entry-point API footgun; name-based lookup unspecified.
- **M (implementation, DISSENT):** AllOf group-hash overhead; cancel fanout mechanism missing; policy_variant wire compactness; metric label anti-goal; replay mechanics mirror K.D2.

### Round 1 revisions (commit 4985d6d)
- §2.1 worked examples, §3 sparse storage + counter clarification, §4 Invariant Q6, §4.1 partition-batched cancel, §5 LetRun-under-impossible, §6.1 tightened ordering + dropped sugar, §6.2 lookup note, §6.3 string trade-off, §7 stuck-group reason + label anti-goal, §8.4 transient tolerance, §8.5 persisted pending + reconciler, §9 item 3 benchmark commitment, §11 Stage B/C aligned.

### Round 2 — L ACCEPTs; K and M narrow dissents on new content

- **K.D6 / M.D6:** batch per-id disposition contract + Lua error semantics; dispatcher coalescing across concurrent groups.
- **K.D7 / M.D7 (shared):** reconciler full-scan cost → per-partition index SET (matches project SCAN→SETs stance).

### Round 2 revisions (commit 35bab8e)
- §4.1 dispatch contract + coalescing, §6.3 `ff:pending_cancel_groups:{p:N}` index SET, §8.5 reconciler iterates SET not scan, §11 Stage C benchmark extended to ≥500 concurrent groups.

### Round 3 — Unanimous ACCEPT
All three challengers (K, L, M) confirm no residual concerns on the revised RFC.

## Design locks (reaffirmed, not relitigated)

- `OnSatisfied::{CancelRemaining, LetRun}`, default `CancelRemaining` — owner lock preserved.
- Threshold / weighted joins — out of scope, deferred to RFC-017.
- `AnyOf` kept as named variant, not collapsed into `Quorum { k: 1 }` (§10.1).

## Open questions remaining (§9) — explicitly not blocking ACCEPT

1. `AnyOf { LetRun }` real-world use — consumer-feedback question, revisit 6mo.
2. `sibling_quorum_impossible` under `LetRun` — pure LetRun semantics locked for v1.
3. Hard `n` cap — Stage C benchmark will inform; soft cap (128) + observability metric is the v1 stance.

Ready for owner adjudication.
