# RFC-013 Round-2 — M (implementation / feasibility)

**Bottom line:** ACCEPT

Re-reviewing impl feasibility after §9.2 rewrite.

## Round-1 items verification

- **M1** §9.2 Lua delta: fully rewritten with explicit steps 1-3 (early dedup GET branch, existing logic unchanged on miss, post-commit HSET + PEXPIRE). GREEN.
- **M2** KEYS/ARGV slot accounting: §9.2 pins 17→18 KEYS, 17→19 ARGV, lists the new slots explicitly. Backend `slot_count` assertions updated in lockstep. GREEN.
- **M3** `resume_delay_ms` wire path: §9.2 confirms it rides today's `resume_policy_json` ARGV slot — no new wire position. GREEN.
- **M4** `InvalidLeaseForSuspend` existence: §4 now pins it at `crates/ff-core/src/engine_error.rs:183`. GREEN.
- **M5** `build_suspend_args` body: §5.1 spells out the defaults — UUIDv4 `suspension_id`, `WaitpointBinding::fresh()` for strict, `use_pending(...)` for try-on-pending, `TimeoutBehavior::Fail` default, `now = SystemTime::now()`, `requested_by = Worker`, `continuation_metadata_pointer = None`, `idempotency_key = None`. GREEN.
- **M6** `PendingWaitpoint` → `UsePending` mapping: §5.1 confirms HMAC token lookup is Lua-side, consistent with RFC-004 §Waitpoint Security. GREEN.
- **M7** Backend-serializer + scanner tests: §9.3 item 3 adds both — the byte-for-byte legacy-SDK comparison is exactly the right canary. GREEN.

## Per-section impl-feasibility re-scan

- §2.2 GREEN — type surface is buildable; constructors are thin delegates.
- §2.2.1 GREEN — impl cost is a few dozen LOC in `ff-core/src/contracts/mod.rs`.
- §2.3 GREEN.
- §2.4 GREEN — cross-field validation is a Rust-side pre-FCALL check, trivial to implement.
- §2.5 GREEN — `ResumePolicy::normal()` is a constant-folded const expression in impl terms.
- §2.6 GREEN — enum-to-string serialization matches what the backend already does for `DeliverSignalArgs`.
- §3 GREEN.
- §4 GREEN — only one new variant (`StateKind::AlreadySatisfied`), which is a single-line enum addition in `engine_error.rs`.
- §5.1 GREEN — `stop_renewal` move is a trivial reorder; `SuspendOutcomeDetails` is a 4-field struct.
- §9.1 GREEN — landing order + type locations consistent.
- §9.2 GREEN — Lua delta is well-scoped. The +30 LOC estimate is realistic. Serialized-outcome version-pin note is the right forward-compat hedge.
- §9.3 GREEN — test matrix now covers backend-internal serializer canary AND idempotency TTL cases.
- §9.4, §9.5 GREEN.

## One micro-note, NOT a dissent driver

§9.2's "Total new Lua LOC: ~30" is an estimate that should be verified in impl. If it exceeds ~60 LOC there's a refactoring discussion about factoring dedup into a helper — but that's impl-time scope, not RFC scope.

No remaining dissent.
