# RFC-013 Round-1 — M (implementation / feasibility)

**Bottom line:** DISSENT

Role: engineer implementing the Lua + Rust changes. I care about wire cost, scanner interaction, migration mechanics.

## Per-section

- §Summary GREEN.
- §Motivation GREEN.
- §Assumptions GREEN — A2 (HMAC unchanged) and A4 (`create_waitpoint` unchanged) are the load-bearing claims that keep the wire diff small.
- §1 GREEN — line pins match my reading of the tree.
- §2.1 GREEN.
- §2.2 RED { **`idempotency_key` implementation cost is underestimated in §9.2 ("None required").** Per §2.2 doc, Lua must: (a) compute dedup hash key `ff:dedup:suspend:{p:N}:<execution_id>:<idempotency_key>`, (b) GET it BEFORE the `already_suspended` check, (c) if HIT: return serialized `SuspendOutcome` verbatim (skipping all state mutation), (d) if MISS: proceed with suspend, then SET the hash with TTL = suspension-timeout-ceiling + grace, serialize `SuspendOutcome` into it. This is a non-trivial Lua patch: ~30 LOC added to `ff_suspend_execution`, plus the serialized-outcome format needs to be stable across Lua versions (or the dedup entry becomes unparseable after a Lua upgrade). §9.2's "none required" is flat wrong. Flip-fix: rewrite §9.2 to specify the Lua delta and pin the serialized outcome format (length-prefixed positional array matching today's `parse_suspend_result`? Or JSON?). }
- §2.2 RED { **KEYS/ARGV slot accounting is unspecified for the idempotency_key path.** Today's `ff_suspend_execution` uses 17 KEYS + 17 ARGV per §1. Adding dedup needs: +1 KEY (dedup hash) +1 ARGV (idempotency_key string, empty when None) +1 ARGV (dedup TTL). That's 18/19 slots. Need explicit spec so backend's `slot_count` assertion doesn't drift silently. Flip-fix: §9.2 enumerate the new KEYS/ARGV positions. }
- §2.3 GREEN — outcome enum shape is fine.
- §2.4 GREEN — matches the existing `ff_suspend_execution` `initialize_condition` helper's inputs.
- §2.5 YELLOW { `ResumePolicy` has 5 fields; most map to existing `resume_policy_json` ARGV fields. But `resume_delay_ms: Option<u64>` — RFC-004 §Resume policy says "delay before execution becomes eligible". Is this carried in the existing Lua contract? If so, which ARGV slot? If not, it's a new write — §9.2 should call it out. Flip-fix: verify against current `ff_suspend_execution` and either confirm no-Lua-change or add to §9.2's delta. }
- §2.6 GREEN — enum-to-string serialization is a trivial backend concern.
- §3 GREEN from an impl POV (given §9.2 is corrected).
- §4 YELLOW { `Validation(InvalidLeaseForSuspend)` — RFC says it was added in Round-4. Confirm this variant exists in current `EngineError::Validation` kind enum. If it doesn't, this RFC has to land it; not a blocker, just an accuracy check. Flip-fix: grep `InvalidLeaseForSuspend` and either leave as-is (if landed) or note it as a new variant in §4's "New kinds needed" bullet. }
- §5.1 YELLOW { **`build_suspend_args` helper is called in §5.1's strict `suspend` body but not defined.** What does it do — construct a `SuspendArgs` with SDK-minted UUIDs for `suspension_id`/`waitpoint_id`/`waitpoint_key`, pack `timeout` into `timeout_at`+`timeout_behavior`, wire up `now` from `SystemTime::now`, and default `requested_by: Worker`, `continuation_metadata_pointer: None`, `idempotency_key: None`? That's a lot of implicit defaulting. Flip-fix: spell out `build_suspend_args`'s body OR point at a helper that the SDK ships. Also: does the SDK always default `idempotency_key: None`? If so, the strict-suspend retry contract per §3.3 is "never idempotent unless caller overrides". Document this in §5.1. }
- §5.1 YELLOW { `try_suspend_on_pending` takes `&PendingWaitpoint`. What's the type? `create_waitpoint` returns something — presumably a struct with `waitpoint_id: WaitpointId` plus HMAC token. The SDK destructures this into `WaitpointBinding::UsePending { waitpoint_id }`. Is the HMAC token also fed in, or does the backend look it up from the waitpoint hash? Per §2.2 doc: "`UsePending` — The key + HMAC token were returned from that call; `suspend` resolves them from the waitpoint hash." OK, Lua looks it up. Then why does `create_waitpoint` hand the token back? Flip-fix: one-line note in §5.1: "`PendingWaitpoint` carries the `waitpoint_id` the SDK uses to build `WaitpointBinding::UsePending`; the HMAC token is resolved Lua-side from the partition waitpoint hash at `suspend` time." }
- §5.2 GREEN.
- §5.3 GREEN.
- §6 GREEN.
- §7 GREEN.
- §8 GREEN.
- §9.1 GREEN — landing order sequences types → trait → backend → SDK, which matches how Round-7 PRs shipped.
- §9.2 RED { "None required" is incorrect once `idempotency_key` is counted — see §2.2 RED. Rewrite. }
- §9.3 YELLOW { Integration tests list `suspend` against a live Valkey. Migration plan doesn't call out that the backend-side `ResumeCondition → ARGV JSON` serializer needs its own unit tests to catch schema drift. Flip-fix: add to §9.3: "Backend-internal serializer: for each `ResumeCondition` variant, assert the emitted ARGV JSON matches the exact string the legacy SDK produced, so the Lua no-change claim survives schema tweaks." }
- §9.3 YELLOW { Scanner interaction is not tested. RFC-004's timeout scanner polls suspensions and fires timeout behaviors at `timeout_at`. This RFC doesn't change the scanner. But: `ResumeCondition::TimeoutOnly` + `timeout_behavior: Escalate` is a new-ish combo that the scanner may not exercise today. Flip-fix: add a scanner-integration test row to §9.3, OR state explicitly "scanner tests unchanged; existing RFC-004 scanner tests cover all `TimeoutBehavior` variants." }
- §9.4 GREEN — risk call-out matches impl scope.
- §9.5 GREEN — rollback plan is clean, zero durable-state changes.

## Flip-to-ACCEPT changes

1. **§9.2**: rewrite from "None required" to the explicit Lua delta for `idempotency_key` dedup (new KEYS/ARGV slots, dedup hash GET/SET, serialized outcome format, TTL computation).
2. **§9.2**: verify `resume_delay_ms` path — is it a new ARGV or already carried?
3. **§4**: confirm `InvalidLeaseForSuspend` already exists in `ValidationKind`.
4. **§5.1**: spell out `build_suspend_args` body (or define it as SDK-internal helper with stated defaults: `requested_by=Worker, idempotency_key=None, continuation_metadata_pointer=None, now=SystemTime::now`).
5. **§5.1**: one-line on `PendingWaitpoint` → `WaitpointBinding::UsePending` mapping and HMAC-token Lua lookup.
6. **§9.3**: backend-internal ARGV-JSON serializer schema tests; scanner-interaction test coverage confirmation.
