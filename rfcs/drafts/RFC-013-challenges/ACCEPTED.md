# RFC-013 — Unanimous ACCEPT

**Reached:** round 2 (2026-04-23)
**Final commit:** `12106f6` on branch `rfc/013-stage-1d`
**PR:** #211 (open; owner review pending)

## Rounds

### Round 1 — all three DISSENT
- **K** (skeptic): 7 correctness clarifications (idempotency_key semantics, TTL cap, timeout-only-no-deadline validation, error rows, describe-retry, §9.2 Lua delta accuracy, TTL-expiry test).
- **L** (consumer): 9 ergonomics items (constructors per `non_exhaustive_needs_constructor` memory rule, clock-skew doc, waitpoint_key cross-field validation, SDK auto-reconcile contract, `StateKind` non-exhaustive confirmation, `SuspendOutcomeDetails` definition, `stop_renewal` ordering / lease bleed).
- **M** (impl): 7 feasibility items (§9.2 Lua delta, KEYS/ARGV slot accounting, `resume_delay_ms` wire path, `InvalidLeaseForSuspend` citation, `build_suspend_args` body, `PendingWaitpoint` → `UsePending` HMAC lookup, backend-serializer + scanner tests).

**Author response:** conceded all 23 findings. No push-back. Revisions applied in commit `12106f6`.

### Round 2 — all three ACCEPT
- **K** ACCEPT: every round-1 item addressed with specific text; no new concerns.
- **L** ACCEPT: constructors + ergonomics fixes land; `stop_renewal` reorder correct; no new concerns.
- **M** ACCEPT: §9.2 Lua delta now accurate; slot counts and test matrix comprehensive; no new concerns.

## Material changes during debate

1. **§2.2** — `timeout_at` gets explicit clock-skew tolerance doc (10 min) + pointer to `server_time()` helper.
2. **§2.2** — `idempotency_key` dedup TTL formula pinned: `min(timeout_at - now + SUSPEND_DEDUP_GRACE_MS, SUSPEND_DEDUP_MAX_TTL_MS=7d)`.
3. **§2.2** — dedup-hit `suspension_id` echo contract made explicit (first-call's id wins).
4. **§2.2.1** (new subsection) — constructors: `SuspendArgs::new` + `with_*` chain, `WaitpointBinding::fresh()` / `use_pending()`, `ResumePolicy::normal()`. Satisfies `non_exhaustive_needs_constructor`.
5. **§2.4** — added `waitpoint_key` cross-field invariant (`WaitpointBinding::Fresh` vs `ResumeCondition::Single`).
6. **§3.1** — describe-retry budget note added.
7. **§4** — four new error-table rows (TimeoutOnly+no-deadline, waitpoint_key mismatch, UsePending-already-activated, handle-kind-precheck-not-dedup-dodgeable); `StateKind`/`ValidationKind` non-exhaustive confirmation with file-line pins; `InvalidLeaseForSuspend` pin.
8. **§5.1** — `stop_renewal` reordered to fire only on committed `Suspended` outcome (no lease bleed on transport error); `SuspendOutcomeDetails` defined; `build_suspend_args` defaults spelled out; `PendingWaitpoint` → `UsePending` mapping clarified; no-SDK-auto-reconcile contract stated.
9. **§9.2** — rewritten from "none required" to explicit Lua delta: +1 KEY (dedup hash) / +2 ARGV (idempotency_key, dedup_ttl_ms), 17→18 / 17→19 slot counts, three-step dedup branch, serialized-outcome version pin.
10. **§9.3** — added tests: idempotency TTL expiry, `timeout_at == None` TTL cap, cross-field validation, TimeoutOnly-no-deadline validation, constructor round-trip, backend-internal ARGV-JSON serializer canary, scanner-interaction confirmation.

## No design changes

All 10 material changes are clarifications, constructors, or taxonomy tightening. The design decisions (typed `SuspendArgs`/`SuspendOutcome`, `ResumeCondition` enum with `Composite` reservation, `suspend` / `try_suspend` split, `idempotency_key` field) landed in round 0 and survived both debate rounds unchanged.

Owner adjudications from 2026-04-23 (try_suspend split, flat composites dropped, `idempotency_key`) were not re-litigated — challengers respected the overrule carve-out.
