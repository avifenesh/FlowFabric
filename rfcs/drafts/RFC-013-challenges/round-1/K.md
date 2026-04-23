# RFC-013 Round-1 — K (skeptic / correctness)

**Bottom line:** DISSENT

## Per-section

- §Summary GREEN — problem statement is clear; scope bounded.
- §Motivation GREEN — breakage list is concrete, ties to specific lines.
- §Assumptions YELLOW — A1 says "enforcement is runtime, not compile-time — same model as complete/fail" but does NOT specify what the runtime error is when a caller passes a Suspended-kind handle *into* a pre-suspend op. §4 table covers `suspend` on Suspended handle → `State(AlreadySuspended)`, but the general inverse (e.g. `complete(&suspended_handle)`) is out of RFC-013 scope. Flag only — not a dissent driver.
- §1 GREEN — file/line pins are precise.
- §2.1 GREEN — signature matches round-7 precedent.
- §2.2 RED { **`suspension_id` uniqueness semantics are unspecified when `idempotency_key` is set.** §2.2 says "`suspension_id` is worker-minted" and §3.3 says "`suspension_id` is echoed ... not for Lua-side dedup". But §2.2's idempotency_key note says "the first outcome is returned verbatim (including the original `suspension_id` ...)". Question: if the caller mints a NEW `suspension_id` on the retry but passes the SAME `idempotency_key`, does the backend (a) return the ORIGINAL `suspension_id` from the first call, or (b) return a mismatch error? The RFC implies (a) by "verbatim", but that means `suspension_id` on the wire is a *request* id that the backend may silently rewrite. That's surprising — callers who persist `suspension_id` pre-call to correlate logs will see a mismatch between what they sent and what came back. Needs one sentence clarifying this is the contract. Flip-fix: add a sentence to §2.2's idempotency_key doc: "On dedup hit, the returned `suspension_id` is the FIRST call's minted id, not the retry's — callers correlating by `suspension_id` should use the echoed value, not the sent value." } 
- §2.2 RED { **`idempotency_key` dedup window vs timeout_at semantics are underspecified.** RFC says "TTL = suspension-timeout ceiling + grace". But `timeout_at` is `Option<TimestampMs>` — `None` means "suspend indefinitely". What is the dedup TTL when `timeout_at == None`? Worst case is unbounded, which is a memory leak in the dedup hash namespace. Flip-fix: specify a dedup TTL cap (e.g. `min(timeout_at - now + grace, DEDUP_MAX_TTL = 7d)`), and state what `None` falls back to (e.g. `DEDUP_MAX_TTL`). } 
- §2.3 GREEN — enum shape is sound; handle substitution semantics clear.
- §2.4 YELLOW { **`ResumeCondition::TimeoutOnly` + `timeout_at == None` is a contradiction.** If resume is timeout-only and no timeout is set, the exec suspends forever with no satisfier. Validation should reject this combo. Flip-fix: add to §4 error table: `ResumeCondition::TimeoutOnly` + `timeout_at.is_none()` → `Validation(InvalidInput { detail: "timeout_only_without_deadline" })`. }
- §2.5 GREEN.
- §2.6 GREEN.
- §3.1 RED { **"describe-and-reconcile on any retry" creates an unbounded error recovery loop if `suspend` transport-errors and `describe_execution` also transport-errors.** The contract says caller MUST describe before retrying. But what if the describe call itself fails transiently? The RFC doesn't spec a retry budget. This is not a blocker for correctness, but it's a trap for implementers. Flip-fix: add a note to §3.1 contract: "Callers may retry `describe_execution` independently under normal transport-retry policy; `suspend` remains un-retryable until describe succeeds. With `idempotency_key`, this problem is moot." } 
- §3.2 GREEN — invalid replay cases enumerated.
- §3.3 GREEN after §2.2 fixes above.
- §4 RED { **Missing row: `suspend` on an exec whose pending waitpoint (`UsePending`) has already been *activated* by a racing `suspend`.** Two workers on the same exec (which Round-4 disallows by lease, so probably impossible), OR one worker's first `suspend` committed and is now active, and a retry with `UsePending { waitpoint_id }` references the same waitpoint. What does Lua return? Probably `already_suspended` at the exec level before checking waitpoint state. Should be listed explicitly. Flip-fix: add a row: "`UsePending` waitpoint already activated by a prior committed suspend" → `State(AlreadySuspended)`, noting the check order (exec-level before waitpoint-level). }
- §4 RED { **`HandleKind::Suspended` pre-check is described in §3.2 case 3 but not in §4 error path.** §3.2 says backend pattern-matches handle kind and returns `AlreadySuspended` without FCALL. The §4 table says "Handle kind is Suspended at entry → State(AlreadySuspended), Rust-side check, no FCALL." That's consistent. But the table also needs to say: this is a *bug detection*, not a retry-safe error — it is NOT retryable even with `idempotency_key`, because the key is namespaced on `(partition, execution_id, idempotency_key)` but the handle is already in an unusable kind. Clarify classification. Flip-fix: add a note to that row: "Not de-duped by `idempotency_key`; this is a pre-FCALL Rust-side rejection that fires before dedup lookup." }
- §5.1 GREEN — split is sound, rationale crisp.
- §5.2 GREEN.
- §5.3 GREEN.
- §6 GREEN.
- §7 GREEN — remaining open questions are appropriately scoped.
- §8.1-§8.6 GREEN.
- §9.1 GREEN.
- §9.2 GREEN — no Lua changes is a claim I want to verify. §2.2's idempotency_key note says "Lua stores a hash `ff:dedup:suspend:{p:N}:<execution_id>:<idempotency_key>` → serialized `SuspendOutcome`". That IS a Lua change (new write + read + TTL management inside `ff_suspend_execution`). §9.2 saying "None required" contradicts §2.2. Re-classify: **§9.2 RED**. Flip-fix: change §9.2 to "Lua adds a partition-scoped dedup hash write/read around the `already_suspended` check. The KEYS/ARGV contract grows by 2 keys (the dedup hash, and a TTL arg). Existing callers not passing `idempotency_key` pass empty-string ARGV; Lua no-ops the dedup branch."
- §9.3 YELLOW { Test matrix doesn't cover the idempotency_key dedup-TTL-expiry case: call twice with same key, wait past TTL, call third time — third should act fresh. Flip-fix: add a third idempotency case to §9.3 item 1. }
- §9.4 GREEN.
- §9.5 GREEN.

## Flip-to-ACCEPT changes

Minimal edits to the RFC:

1. **§2.2** (`idempotency_key` doc): add the "dedup hit returns FIRST call's suspension_id" clarification.
2. **§2.2** (`idempotency_key` doc): add dedup TTL cap for `timeout_at == None`.
3. **§2.4**: add `TimeoutOnly` + no-deadline validation row.
4. **§4** error table: add `UsePending` already-activated row; add note on `HandleKind::Suspended` pre-check not dedup-dodgeable.
5. **§3.1**: add one-line note on describe-retry budget.
6. **§9.2**: correct the "none required" claim — dedup hash is a new Lua write.
7. **§9.3**: add idempotency TTL-expiry test.

All are clarifications; no design change.
