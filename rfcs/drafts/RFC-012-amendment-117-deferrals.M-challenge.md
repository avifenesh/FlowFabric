# Round-3 CHALLENGE against RFC-012 amendment #117 deferrals (draft v3)

Reviewer: Worker M2. Base: `da89fa9` (worktree at `23d3ce3` — v3 citations still match). Read-only.
K (round-1) + L (round-2) findings not re-litigated. Focus: flaws in v3's resolutions + net-new hazards v3 introduced.

---

## MUST-FIX

### #M1: SDK's own `parse_suspend_result` constructs `SuspendOutcome::Suspended` EXHAUSTIVELY — v3's "handle synthesis post-FCALL" story doesn't fit this site

v3 §R7.2.2 line 127 states: "Backend-side handle synthesis. Lua wire returns no handle in the FCALL response today. The backend impl synthesises a `Handle { kind: HandleKind::Suspended, … }` post-FCALL from `suspension_id` + `execution_id` + `backend_tag`."

v3 §R7.3 bullet 3 moves `SuspendOutcome` into `ff-core::backend` and bullet 6 adds `pub use ff_core::backend::SuspendOutcome` in `ff-sdk::task`. §R7.2.2 line 128 claims "every `SuspendOutcome::Suspended` match in `crates/` uses `{ .. }` rest-patterns or wildcard arms."

The v3 argument is about `match` sites. It skips `construct` sites. Evidence:

`crates/ff-sdk/src/task.rs:1557-1562`:
```rust
Ok(SuspendOutcome::Suspended {
    suspension_id,
    waitpoint_id,
    waitpoint_key,
    waitpoint_token,
})
```

This is the SDK's internal wire parser (`parse_suspend_result` at `:1465`), called by the SDK-level `ClaimedTask::suspend` forwarder (`:818`) which v3 §R7.3 bullet 2 says "stays direct-FCALL until Stage 1d." So the SDK constructor site remains live AND exhaustive, AND it has no `Handle` to supply — `parse_suspend_result` is a pure wire parser with no access to `backend_tag` / `lease_id` / the rest of the Handle's opaque payload (`backend.rs:60-69`).

Consequence for v3 as drafted:
1. Adding `handle: Handle` as a required field on `SuspendOutcome::Suspended` in the moved type breaks `task.rs:1557` compile.
2. Making `handle` optional (`Option<Handle>`) re-weakens the contract v3 says it is preserving.
3. Splitting construction paths (ff-core variant keeps handle, SDK keeps old shape) defeats the `pub use` shim — the shim's entire purpose is path-preservation with a single canonical type.

Similar exhaustive construction at `crates/ff-sdk/src/task.rs:1550-1555` for `AlreadySatisfied` — not affected (no new field there), but the pattern confirms the SDK parser doesn't use rest-patterns in constructors.

**Required fix.** Either (a) drop `handle` from `Suspended` and acknowledge the trait's existing `HandleKind::Suspended` contract is not preserved end-to-end until Stage 1d collapses the SDK forwarder (owner decision under §R7.6.7), OR (b) keep `handle` but spell out in §R7.3 that bullet 2 grows: the SDK `suspend` forwarder at `task.rs:812-818` (including `parse_suspend_result` at `:1465`) must be rewritten in the same PR — which contradicts "suspend site stays direct-FCALL until Stage 1d." This tension is load-bearing for §R7.6.7's (b)-lean: the divergence v3 owns forces partial SDK migration anyway.

### #M2: v3 cites wrong line numbers for `SuspendOutcome` match sites

v3 §R7.2.2 line 128 claims in-tree `SuspendOutcome::Suspended` matches at "`task.rs:2157,2170` (wildcard-armed — still compile)". Those lines are `ReportUsageResult::SoftBreach` and `ReportUsageResult::HardBreach` match arms in `parse_report_usage_result` tests (verified at `crates/ff-sdk/src/task.rs:2157,2170`), NOT `SuspendOutcome` sites.

Actual in-tree `SuspendOutcome::Suspended` match sites:
- `crates/ff-readiness-tests/tests/waitpoint_hmac_roundtrip.rs:337-342` — uses `{ .. }` rest + `other => panic!(..)` arm. Safe against new field.
- `crates/ff-test/tests/resume_signals_integration.rs:143, 213, 286` — all use `{ waitpoint_id, waitpoint_key, waitpoint_token, .. }`. Safe.
- `crates/ff-test/tests/e2e_lifecycle.rs:5476-5482` — uses `{ …, .. }` rest + explicit `AlreadySatisfied { .. }`. Safe.

So the match-sites claim (every match is rest-patterned) is CORRECT after re-verification, but the cited evidence is wrong. This matters because K's whole `#[non_exhaustive]` audit discipline depends on accurate match-site enumeration; a reviewer chasing v3's citation lands on unrelated code.

**Required fix.** Replace line-128 citations with the actual sites above; keep the conclusion, which holds.

### #M3: `ReportUsageResult` gains `#[non_exhaustive]` but v3 does not specify ordering vs return-type replace

v3 §R7.2.4 / §R7.3 bullet 4 do two things to `ReportUsageResult` in the same PR:
1. Add `#[non_exhaustive]` at `contracts.rs:1400` (K1 fix).
2. Become the replacement return type for `report_usage` (core delta).

Ordering question v3 doesn't answer: if the PR lands in separate commits — attribute-first or trait-signature-first — both are fine in isolation because `ReportUsageResult` is already pub and already used by SDK parsers. But v3 should say the landing is atomic (single commit) to avoid a reviewer assuming "add attribute, wait for dependent minor, then cut trait change" — which is a semver-two-step that contradicts the 0.4.0 framing. Not a blocker, but the CHANGELOG entry (§R7.3 line 234) bundles both deltas, so the landing plan should too.

**Required fix.** One sentence in §R7.3 or §R7.7: "All five consumer-update bullets land in a single commit / single 0.4.0 release cut; attribute + return-type are one breaking-change event."

---

## DESERVES-DEBATE

### #MD1: Semver lane — v3 says "0.4.0" in CHANGELOG but §R7.1 / §R7.4 framing is "pre-1.0 posture, CHANGELOG-only communication"

v3 §R7.3 CHANGELOG entry reads `(0.4.0)`. v3 line 5 says "Pre-1.0 posture: CHANGELOG-only communication, no BC shims beyond `pub use` path-preservation." Today's released line (per `git log`) is 0.3.3-hotfix.

Two questions v3 should decide, not leave implicit:
1. Does this amendment ship in 0.3.x (minor bump over 0.3.3) or 0.4.0 (breaking bump)? Rust-ecosystem convention: breaking change on a public trait pre-1.0 is a minor bump (0.3 → 0.4). `EngineBackend` is pub. v3's CHANGELOG line says 0.4.0, which is correct under that convention — but §R7.1's "round-7 amendment" framing and Stage-1b framing don't connect to the release cut.
2. Does `suspend` shape-reservation (§R7.6.7 (a)) still justify a 0.4.0 cut, or does deferring `suspend` (b) let the other three land in a 0.3.x minor? The answer depends on whether `append_frame`'s `() → AppendFrameOutcome` is itself trait-breaking enough to force 0.4.0.

It is: `report_usage` replace (`AdmissionDecision → ReportUsageResult`) is unambiguously breaking. So the release cut is 0.4.0 regardless of §R7.6.7. Say so explicitly.

**Debate question.** Bless "lands in 0.4.0 regardless of §R7.6.7 outcome" in §R7.7 to collapse this question.

### #MD2: `AppendFrameOutcome` derive widen to `PartialEq, Eq` — field coverage verified, but `stream_id: String` semantic-equality is a shape commitment

v3 §R7.2.1 line 60-65:
```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendFrameOutcome { pub stream_id: String, pub frame_count: u64 }
```

Both fields trivially impl `PartialEq + Eq` (String, u64). Compile-safe. But `stream_id` is a Valkey Stream entry id (e.g. `1234567890-0` per XADD semantics). `PartialEq` on it is byte-equality; in downstream assert-eq tests that's almost always what the author wants. Concern: if a future wire change normalises `stream_id` (e.g. drops the `-0` suffix for sequence-0 entries, or adopts a typed `StreamId` newtype), the canonical-derive commitment locks the field at `String`. v3 should note in §R7.5.6 that `stream_id: String` is the stable shape assumption and a future typed-stream-id migration would be its own breaking change.

Soft concern, not a blocker. Derive widen is correct.

### #MD3: §R7.6.7 should be closed by this amendment, not escalated

v3 provisionally leans (b) (defer `suspend`) and flags "owner must endorse before landing." But the in-tree evidence is now complete enough to decide without owner input:

- #M1 establishes that option (a) (shape-reservation) requires SDK `parse_suspend_result` rewrite in the same PR OR an Option-typed handle that weakens the contract.
- Option (b) (defer) has no such hazard.
- v3 already leans (b).
- Owner's concerns are structural, not data-driven — and the data now points cleanly to (b).

Escalating (b) to owner on a question the evidence has decided is a coordination tax. Close §R7.6.7 as (b) in v4, note in §R7.7 "owner may reopen if shape-reservation preferred." Same principle as the feedback memory item on deferred design debt — decidable questions shouldn't sit in open-questions lists.

Not quite a must-fix because the escalation has zero cost to correctness; but v3's "Remaining open questions" list at lines 316-324 has 6 items, 3 of which (R7.6.6, R7.6.7, L1 confirmation) are effectively decidable in-tree. Collapse what can be collapsed.

---

## NITS

### #MN1: §R7.7 acceptance gate (3) still describes two tests as one

v3 §R7.7 gate (3): "`_assert_dyn_compatible` + `ValkeyBackend::_dyn_compatible` compile cleanly AND an `Arc<dyn EngineBackend>` runtime smoke test exercising `create_waitpoint` dispatches and returns a valid `PendingWaitpoint`."

Two distinct gates are ANDed into one bullet. Split to (3a) static dyn-safety assertion compiles and (3b) runtime smoke test passes. CI interprets gates bullet-wise; a single failing sub-bullet inside a compound bullet is easy to under-report.

### #MN2: "Envelope prose" LD2 item — §R7.3 bullet 7 is conditional on §R7.6.8, but §R7.6.8 isn't closed

v3 §R7.3 bullet 7: "`rfcs/RFC-012-engine-backend-trait.md` §3.1 — relax envelope prose per LD2/§R7.6.8 to admit gap-fill adds alongside splits-for-honesty."

v3 §R7.6.8 says "if amended, §R7.3 bullet 7 applies; if not, drop bullet 7."

As written, §R7.3 bullet 7 is conditional text inside a normative landing-plan list. Readers scanning §R7.3 will count 7 items; readers scanning §R7.6.8 will see it's optional. Make §R7.3 bullet 7 explicitly conditional ("(§R7.6.8-conditional) RFC-012 §3.1 prose relaxation …") or move to §R7.6.8 and note §R7.3 grows by 1 bullet on owner elect.

### #MN3: `AppendFrameOutcome` absence of `#[non_exhaustive]` deserves a one-line explicit rationale at §R7.2.1

v3 line 67 says "No `#[non_exhaustive]` (matches `FailOutcome` comment at `backend.rs:546-550`). Diverges from v1's silent addition (K3)." Good — but the `FailOutcome` precedent comment explains the choice ("existing consumers construct and match exhaustively") and `AppendFrameOutcome` does not inherit that justification automatically: today there are no `AppendFrameOutcome`-construction sites outside the SDK parser (`task.rs:1747`). v3 is relying on consistency-with-FailOutcome rather than consumer-shape evidence. State one line: "Current `AppendFrameOutcome` construction is internal to `ff-sdk`; future external constructors are not anticipated, matching `FailOutcome`'s consumer-shape rationale." Keeps the precedent defensible under later scrutiny.

---

## Affirmative no-findings

- **L1 resolution — `handle` divergence ownership.** v3 owns the divergence (§R7.2.2 lines 124-128), cites the trait's existing Round-4 contract correctly (verified at `engine_backend.rs:126-128` and `backend.rs:50,788`), and spells out the `pub use` shim's construction-contract limitation. Conceptually sound. See #M1 for the concrete SDK-parser construction site that escalates this into a Stage-1b-scope problem.
- **L2 resolution — `create_waitpoint` naming.** v3 recants v2's "pending is Valkey-internal" framing, cites Rust + Lua "pending" evidence correctly (verified at `ff-script/src/error.rs:170,403-408` and `flowfabric.lua:801-823,3641,3690`), and documents the pending-vs-active contrast in the method docstring (§R7.2.3). The layered-design argument (trait describes caller's business op) is coherent. Option (a) / (b) / (c) all remain defensible; (c) is justified. Not impedance mismatch.
- **LD2 / LD3 resolutions.** §R7.5.6 canonical-derive posture is clean. §R7.6.8 escalates the envelope-prose question correctly (this one IS an owner call — §3.1's "in spirit" is editorial discretion).
- **K1, K2, K3, K4, K5 resolutions.** All confirmed fixed per L's round-2 affirmative no-findings; re-verified against code at `da89fa9`:
  - `ReportUsageResult` lacks `#[non_exhaustive]` today (`contracts.rs:1400`); v3 adds it (§R7.3 bullet 4). Correct.
  - `SuspendOutcome::{Suspended, AlreadySatisfied}` carry `suspension_id` (`task.rs:76,85`). Correct.
  - `FailOutcome` shape at `backend.rs:546-550`: `Clone, Debug, PartialEq, Eq`, no `#[non_exhaustive]`. Matches v3's canonical-derive statement.
  - `async fn` count is 16 today (`grep -c "^    async fn " engine_backend.rs` = 16). Post-amendment 17. Matches v3.
  - All four stubs return `EngineError::Unavailable`. Matches v3.
- **§R7.5.7 (keep-handle).** Alternatives-considered correctly lists drop-handle as rejected for the right reason (silently weakens Round-4). The alternative is genuinely worse — even though the keep-handle path has its own hazard (#M1), the analysis of the alternative is sound.
- **`SuspendOutcome` variants exhaustiveness.** The Lua wire at `ff-script/src/functions/suspension.rs:80-128` has exactly two success arms (`ALREADY_SATISFIED` and the unlabeled ok-path). SDK parser at `task.rs:1549-1563` mirrors exactly those two. `Suspended` + `AlreadySatisfied` are complete; no hidden third arm (`Timeout` is expressed as an error, not a success-outcome variant — correctly).
- **`AppendFrameOutcome` field-level `PartialEq, Eq` coverage.** Both `String` and `u64` impl `PartialEq + Eq` via std. Compile-safe. See #MD2 for a soft shape-commitment concern.
- **Acceptance gate (1) / (2) / (4).** Gate (1) correctly excludes `suspend` per §R7.6.7 (b) lean. Gate (2) is the standard workspace test bar. Gate (4) is CHANGELOG hygiene. Sufficient for this amendment's scope when (a) is rejected. See #MN1 for a sub-bullet split on (3).

---

## Verdict

**Accept with targeted fixes.** v3 resolves L1 / L2 / LD1-3 cleanly at the RFC level. Two residual must-fixes: #M1 (SDK `parse_suspend_result` exhaustive construction site interacts with the moved `SuspendOutcome` + new `handle` field — forces §R7.6.7 toward (b)) and #M2 (citation error on match-site line numbers). #M3 is a one-line landing-atomicity clarification. #MD1-3 are owner-touch items; #MD3 in particular argues v3 can close §R7.6.7 as (b) on the evidence rather than escalate.

Three rounds (K + L + M) is sufficient — #M1 is the last structural finding and is decidable in-place. No need for P.
