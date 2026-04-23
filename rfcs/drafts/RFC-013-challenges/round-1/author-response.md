# RFC-013 Round-1 — Author response

All three challengers DISSENT. Findings categorized below; author position on each, then the applied revisions.

## K (skeptic) — concede all 7 findings

1. **K1 — idempotency_key echoes FIRST call's `suspension_id`.** Concede. Clarify in §2.2 doc.
2. **K2 — dedup TTL cap when `timeout_at == None`.** Concede. Add `DEDUP_MAX_TTL = 7d` cap, document in §2.2.
3. **K3 — `TimeoutOnly` + no deadline is a validation error.** Concede. Add §4 row.
4. **K4a — `UsePending` already-activated waitpoint.** Concede. Add §4 row. Lua check-order: exec-level `already_suspended` first, so this collapses to `State(AlreadySuspended)`.
5. **K4b — `HandleKind::Suspended` pre-check not dedup-dodgeable.** Concede. Add note in §4.
6. **K5 — describe-retry budget.** Concede. Add one-line note in §3.1.
7. **K6 — §9.2 "None required" is wrong.** Concede, 100%. Also flagged by M. Rewrite §9.2 with Lua delta.
8. **K7 — idempotency TTL-expiry test.** Concede. Add to §9.3.

## L (consumer) — concede 9/9 findings

1. **L1 — `SuspendArgs` constructor (blocker).** Concede. Memory rule `non_exhaustive_needs_constructor` applies directly. Add `SuspendArgs::new(...)` with required params + with_* setters in §2.2.
2. **L2 — `WaitpointBinding::fresh()` helper.** Concede. Add to §2.2.
3. **L3 — `ResumePolicy` defaults.** Concede. Add `ResumePolicy::normal()` constructor with documented v1 defaults.
4. **L4 — clock-skew tolerance.** Concede. Add to `timeout_at` doc.
5. **L5 — `waitpoint_key` cross-field validation.** Concede. Add §4 row. `WaitpointBinding::Fresh.waitpoint_key` must equal `ResumeCondition::Single.waitpoint_key` when both present.
6. **L6 — SDK auto-reconcile or caller?** Concede — need to explicitly state: caller owns the describe-and-reconcile loop; SDK does NOT auto-reconcile on transport error of `suspend`. With `idempotency_key`, caller may retry directly.
7. **L7 — `StateKind` is `#[non_exhaustive]`.** Confirmed via grep of `crates/ff-core/src/engine_error.rs:316`. Add confirmation note in §4.
8. **L8 — define `SuspendOutcomeDetails`.** Concede. Define in §5.1.
9. **L9 — `stop_renewal` ordering / lease bleed.** Concede with evidence: §5.1's current order (`stop_renewal` BEFORE FCALL) is wrong. Reorder so `stop_renewal` fires only on successful Suspend outcome (the Suspended handle's lease-state in Valkey is `unowned`, so the renewal loop would be a no-op anyway, but it's cleaner to stop renewal after we KNOW the exec committed to suspended). On transport error, renewal continues — worker's lease survives and the describe-and-reconcile path in §3.1 stays tractable.

## M (impl) — concede all 7 findings

1. **M1 — §9.2 Lua delta.** Concede. Rewrite with explicit new KEYS/ARGV.
2. **M2 — KEYS/ARGV slot accounting.** Concede. Specify +1 KEY (dedup hash) +2 ARGV (idempotency_key, dedup_ttl_ms). New total: 18 KEYS, 19 ARGV.
3. **M3 — `resume_delay_ms` Lua path.** Verified against RFC-004: `resume_delay_ms` is carried today in `resume_policy_json` ARGV slot; no new wire slot needed. Document this in §9.2.
4. **M4 — `InvalidLeaseForSuspend` confirmed.** Grep: `crates/ff-core/src/engine_error.rs:183`. Already present. Note in §4.
5. **M5 — `build_suspend_args` body.** Concede. Define in §5.1 with explicit defaulting.
6. **M6 — `PendingWaitpoint` → `UsePending` mapping.** Concede. One-line note in §5.1.
7. **M7 — backend-serializer + scanner tests.** Concede. Add to §9.3.

## Nothing to push back on

Every finding is either a genuine RFC gap or a valid clarification request. Applying all.

## Revisions applied

Commit: `rfc(013): round-1 revisions addressing K/L/M challenges` — see next push.
