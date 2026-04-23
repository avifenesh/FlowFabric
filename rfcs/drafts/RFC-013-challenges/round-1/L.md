# RFC-013 Round-1 — L (ergonomics / consumer)

**Bottom line:** DISSENT

Role: I am cairn-fabric. I consume `ff-sdk` and pattern-match `EngineError`. I'm reading this RFC cold to decide if the migration from today's `ClaimedTask::suspend` is tractable.

## Per-section

- §Summary GREEN.
- §Motivation GREEN — the "cannot express AlreadySatisfied in Result<Handle, EngineError>" sentence lands.
- §Assumptions GREEN.
- §1 GREEN.
- §2.1 GREEN.
- §2.2 YELLOW { As a consumer, I have to supply `now: TimestampMs` and `timeout_at: Option<TimestampMs>` — both wall-clock caller-side. The RFC says the backend "uses server-time `TIME` for Lua correctness but carries the caller clock for correlation". What clock do I compare against when I compute `timeout_at`? If I use `SystemTime::now()` and the Valkey server clock drifts 10s behind, do I get spurious `Validation(InvalidInput { detail: "timeout_at_in_past" })`? The rustdoc should point me at a helper or at least say "use `backend.server_time()` as the reference clock" if that op exists. Flip-fix: add one doc-comment line on `timeout_at` clarifying clock-skew tolerance on the backend side — e.g. "validated against backend wall-clock with N ms tolerance; worker clock skew within N ms is accepted". }
- §2.2 RED { **`WaitpointBinding::Fresh` forces me to mint `waitpoint_id` AND `waitpoint_key` myself.** That's two UUIDs per suspend call. The rustdoc doesn't say how to mint them — "worker-minted (UUID, `wpk:<uuid>`)" — but does the SDK expose a `WaitpointBinding::fresh()` constructor that does this for me, or am I writing `uuid::Uuid::new_v4()` + `format!("wpk:{}", id)` inline in my handler? Consumer pain: this is ceremony that the SDK should hide. Flip-fix: §5.1 should show a `WaitpointBinding::fresh()` or `SuspendArgs::builder()` helper. Without a constructor, the `#[non_exhaustive]` struct is unbuildable by cairn per our memory rule (`non_exhaustive_needs_constructor`). }
- §2.2 RED { **`SuspendArgs` is `#[non_exhaustive]` with 10+ required fields.** As a consumer, I can't construct it with `SuspendArgs { … }` because of non-exhaustive, and the RFC doesn't specify a builder. This is the exact footgun called out in memory `non_exhaustive_needs_constructor` (applied during v0.3.2 smoke). Flip-fix: §2.2 must specify a constructor — either `SuspendArgs::new(required_fields) + with_timeout(...) + with_idempotency_key(...)` builder, or a `pub fn new(suspension_id, waitpoint, resume_condition, resume_policy, reason_code, requested_by, now) -> Self` with `timeout_at`/`timeout_behavior`/`continuation_metadata_pointer`/`idempotency_key` as setters. This is a landing-PR blocker for me. }
- §2.3 GREEN — the enum is clear; pattern-matching on `Suspended{..}`/`AlreadySatisfied{..}` is what I expect.
- §2.4 YELLOW { From pure rustdoc, `ResumeCondition::Single { waitpoint_key, matcher }` — what's the relationship between `waitpoint_key` here and `WaitpointBinding::{Fresh{waitpoint_key}, UsePending}`? Do they MUST match? If I pass `WaitpointBinding::Fresh { waitpoint_key: "wpk:abc" }` and `ResumeCondition::Single { waitpoint_key: "wpk:xyz", .. }`, is that a validation error? Intuition says yes, but the RFC doesn't say. Flip-fix: add to §4: `WaitpointBinding.waitpoint_key` / `ResumeCondition::Single.waitpoint_key` mismatch → `Validation(InvalidInput { detail: "waitpoint_key_mismatch" })`. }
- §2.5 YELLOW { `ResumePolicy` has 5 bool/option fields, all `#[non_exhaustive]`. Same constructor gap as §2.2. What are the defaults? Is `consume_matched_signals: true` typical? The RFC doesn't say what cairn should pass for "normal" usage. Flip-fix: add either `ResumePolicy::default()` (if derivable) or `ResumePolicy::normal()` convenience with documented defaults, so I'm not copying magic bool values across handlers. }
- §2.6 GREEN — enums are easy to exhaustively match.
- §3 YELLOW { The §3.3 contract — "callers must describe-and-reconcile on any retry without idempotency_key" — means for every suspend call site in cairn I need to wrap with retry+describe logic, OR always pass an `idempotency_key`. That's a big ergonomic ask. Can the SDK wrapper do the describe-and-reconcile for me? §5.1's `suspend`/`try_suspend` don't mention it. Flip-fix: clarify in §5.1 whether the SDK-level `suspend`/`try_suspend` auto-reconciles on transport error, or whether cairn handlers own that loop. If the latter, a §5.4 "consumer retry playbook" subsection would save me re-deriving it. }
- §4 RED { **Error taxonomy maps `Suspend strict + early-satisfied → State(AlreadySatisfied)` but I'm used to matching on `EngineError::{Contention,State,Validation,Transport}`. Is `AlreadySatisfied` a variant I already have, or a new `StateKind` added by this RFC?** §4 says "New kinds needed: one. `StateKind::AlreadySatisfied`". OK that's specified — but cairn currently matches `EngineError::State(kind)` with an exhaustive match. Adding a new variant to a `#[non_exhaustive]` enum is non-breaking; to a non-`#[non_exhaustive]` enum, it breaks every downstream match. The RFC should state whether `StateKind` is `#[non_exhaustive]`. Flip-fix: one-line in §4: "`StateKind` is already `#[non_exhaustive]` per Round-7; adding `AlreadySatisfied` is non-breaking for downstream consumers." (Verify by grep if needed.) }
- §5.1 YELLOW { The `try_suspend` signature returns `TrySuspendOutcome` which is `enum { Suspended(SuspendedHandle), AlreadySatisfied { task, details } }`. What's `SuspendOutcomeDetails`? It's mentioned in the enum body but not defined. From the shape of `SuspendOutcome::AlreadySatisfied` in §2.3 (`suspension_id, waitpoint_id, waitpoint_key, waitpoint_token`), I can infer it. But the RFC should define `SuspendOutcomeDetails` explicitly or point at §2.3. Flip-fix: add a `pub struct SuspendOutcomeDetails { suspension_id, waitpoint_id, waitpoint_key, waitpoint_token }` code block, or inline the fields. }
- §5.1 RED { **The strict `suspend` path in §5.1 says `self.stop_renewal();` runs BEFORE the FCALL, then if the FCALL errors with anything other than AlreadySatisfied, the lease is no longer being renewed but also not released Lua-side.** If the backend call transport-errors, `stop_renewal` has already fired → lease expires at heartbeat timeout → exec drops to reclaimable → worker has no way to re-claim the same exec cleanly. §3.1 says "describe-and-reconcile"; but the lease is bleeding out during that window. Is that acceptable? The cairn handler would expect either (a) the lease survives until FCALL resolution, or (b) the SDK retries the FCALL itself within lease ttl. Flip-fix: §5.1 strict suspend should call `stop_renewal` AFTER the FCALL resolves successfully, not before; OR the RFC should explicitly acknowledge the "lease bleeds during transport error" window as a known limitation with a documented reclaim path. This is a correctness-of-implementation point that affects my handler design. }
- §5.2 GREEN — migration note is clear.
- §5.3 GREEN.
- §6 GREEN.
- §7 GREEN.
- §8 GREEN.
- §9 GREEN.

## Flip-to-ACCEPT changes

1. **§2.2**: add `SuspendArgs::new(...)` constructor (or builder) — blocker per `non_exhaustive_needs_constructor`.
2. **§2.2**: add `WaitpointBinding::fresh()` helper — mints id+key for me.
3. **§2.5**: add `ResumePolicy::normal()` or `Default` impl with documented defaults.
4. **§2.2**: clock-skew tolerance note on `timeout_at` / `now`.
5. **§2.4** / **§4**: waitpoint_key mismatch between `WaitpointBinding` and `ResumeCondition::Single` is a validation error.
6. **§3.3**: pointer to "does SDK auto-reconcile or does caller?"
7. **§4**: confirm `StateKind` is `#[non_exhaustive]`; land `AlreadySatisfied` there.
8. **§5.1**: define `SuspendOutcomeDetails` explicitly.
9. **§5.1**: clarify renewal-stop ordering w.r.t. FCALL (either reorder or document the bleed window).
