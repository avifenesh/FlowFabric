# RFC-013 Round-2 — K (skeptic / correctness)

**Bottom line:** ACCEPT

Re-reviewed revisions after round-1 commits. Every flip-to-ACCEPT item from round-1 was addressed with concrete, specific text. Checked each:

- **K1** (dedup-hit `suspension_id` echo): §2.2 bullet now explicit — "the returned `SuspendOutcome::*.suspension_id` is the first call's minted id, NOT the retry's". GREEN.
- **K2** (TTL cap for `timeout_at == None`): §2.2 specifies `SUSPEND_DEDUP_MAX_TTL_MS = 604_800_000` (7d) cap with the exact min() formula. GREEN.
- **K3** (`TimeoutOnly` + no deadline): §4 row landed with `detail: "timeout_only_without_deadline"`. GREEN.
- **K4a** (`UsePending` already-activated): §4 row explains the check-order collapse to `State(AlreadySuspended)`. GREEN.
- **K4b** (handle-kind pre-check not dedup-dodgeable): §4 has a dedicated paragraph explaining the pre-FCALL rejection fires before dedup lookup and that `idempotency_key` does not help in that case. GREEN.
- **K5** (describe-retry budget): §3.1 has the one-liner noting `describe_execution` may be independently retried under standard transport retry; `idempotency_key` makes this moot. GREEN.
- **K6** (§9.2 "none required" correction): §9.2 completely rewritten — 18 KEYS, 19 ARGV, explicit Lua delta for dedup path, serialized-outcome stability note, resume_delay_ms confirmed on existing slot. GREEN.
- **K7** (TTL-expiry test): §9.3 unit test list has TTL-expiry AND `timeout_at == None` TTL cap test. GREEN.

## Per-section re-scan (looking for new issues introduced by revisions)

- §2.2 GREEN — idempotency_key doc block is longer but coherent.
- §2.2.1 GREEN — constructor set is minimal and addresses the memory rule without bloating the surface.
- §2.4 GREEN — cross-field invariant is the right call.
- §3.1 GREEN.
- §4 error table GREEN — four new rows all map to existing or explicitly-new variants; `StateKind::AlreadySatisfied` is the only net-new addition.
- §5.1 GREEN — `stop_renewal` reorder is correct (only fires on committed Suspend outcome). Comment makes the rationale explicit.
- §9.2 GREEN — serialized-outcome version-pinning note is thoughtful forward-compat hedge.
- §9.3 GREEN — test list is comprehensive.

No new concerns.
