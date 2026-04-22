# Round-2 CHALLENGE against RFC-012 amendment #117 deferrals (draft v2)

Reviewer: Worker L. Base: `da89fa9` (matches draft). Read-only.
K's round-1 findings reviewed; no repetition. Focus: flaws K missed, and
flaws v2 introduced while fixing K.

---

## MUST-FIX

### #L1: `SuspendOutcome::Suspended { handle: Handle, … }` is a NET-NEW FIELD, not "matching SDK" — v2 conflates two separate data-model changes

v2 §R7.2.2 lines 110-116 propose:
```
Suspended {
    suspension_id: SuspensionId,
    handle: Handle,             // ← new-vs-SDK
    waitpoint_id: WaitpointId,
    waitpoint_key: String,
    waitpoint_token: WaitpointToken,
}
```

v2's line 10 framing ("K2 addressed … matching `crates/ff-sdk/src/task.rs:71-90`")
and line 102 ("variant fields match SDK exhaustively") are both **false**.

**Evidence — current SDK shape** (`crates/ff-sdk/src/task.rs:71-90`):
```
Suspended {
    suspension_id: SuspensionId,
    waitpoint_id: WaitpointId,
    waitpoint_key: String,
    waitpoint_token: WaitpointToken,
}
```
No `handle` field today. SDK's `suspend` consumes `self` (`task.rs:813`, docstring
line 806), so there is no handle to return.

The trait's current `suspend` returns bare `Handle` (`engine_backend.rs:129-134`).
v2's proposed enum **merges both** — it introduces a `Handle` field that was
never in the SDK shape.

**Why this matters.** v2 treats this as a neutral move ("matching SDK wire
contract", §R7.2.2 line 132). It is not. Three independent things are happening:
1. Trait widens to typed outcome (fine).
2. SDK exhaustive-construction sites get a new required field (breaking for any
   out-of-tree consumer that mirrors the SDK shape — `pub use` shim preserves
   the path but not the construction contract).
3. `Handle` semantics — the docstring (v2 line 111) says "Returned `Handle`
   supersedes the caller's pre-suspend handle (kind = Suspended)" — is a
   fresh behaviour contract that needs wire verification. `ff_suspend_execution`
   today returns `{1, "OK"|"ALREADY_SATISFIED", suspension_id, waitpoint_id,
   waitpoint_key, waitpoint_token}` per `crates/ff-script/src/functions/suspension.rs:83`
   — **no handle in the wire response.** If the backend impl must synthesise a
   Handle post-hoc, state that explicitly. If not, the `handle` field cannot be
   populated and must drop.

**Required fix.** Either:
(a) Drop `handle` from `Suspended` to actually match SDK and state that the
    trait outcome has no superseding-handle slot (parallels `AlreadySatisfied`
    which also has no handle). SDK forwarder in Stage 1d would then mint a
    Suspended-kind handle locally from the returned fields.
(b) Keep `handle` but acknowledge in §R7.2.2 that (i) SDK construction sites
    grow a new field, (ii) the backend impl synthesises the handle from
    wire-returned `suspension_id` + `execution_id`, not from the FCALL response
    directly, and (iii) the `pub use` shim does NOT preserve the SDK's exhaustive
    constructibility — which contradicts v2 §R7.3's "External consumers: none."
    claim since the shape diverges.

Current v2 framing is not internally consistent.

---

### #L2: `create_waitpoint` rename drops load-bearing "pending" semantics — the Lua wire distinguishes pending vs activated, the trait method name now hides that

v2 §R7.2.3 lines 169-171 justify rename: "'pending' is Valkey-internal
terminology … Generic trait callers don't need 'pending' vs 'active' framing."

**Evidence "pending" is NOT Valkey-internal.** Grep `crates/ff-script/`:
- `crates/ff-script/src/error.rs:170` — `PendingWaitpointExpired` as an
  FCALL error code (`"pending_waitpoint_expired"`).
- `crates/ff-script/src/error.rs:403-408` — TERMINAL error semantics for
  "activating a pending waitpoint whose [state] invalid … pending waitpoint
  is unrecoverable without a fresh one."
- `crates/ff-script/src/flowfabric.lua:801-823` — `validate_pending_waitpoint`
  distinguishes pending-but-not-activated state from a suspension-activated
  waitpoint; `pending_waitpoint_expired` is a distinct error arm.
- `crates/ff-script/src/flowfabric.lua:3641, 3690` — `use_pending_waitpoint`
  as a first-class ARGV flag on `ff_suspend_execution`.

There are TWO states: (1) pending waitpoint (token issued, not yet backing a
suspension) and (2) active waitpoint (bound to a suspension). `create_waitpoint`
creates only the **pending** kind. The active kind is created by `suspend`.

**Why the rename is a footgun.** A consumer reading `EngineBackend` sees
`create_waitpoint(...)` and `suspend(..., waitpoints: Vec<WaitpointSpec>, ...)`.
Which one creates "the waitpoint"? Both. The SDK method name
(`create_pending_waitpoint`) disambiguates; the trait rename makes the two
appear to compete. The collision isn't `issue_claim_grant` (a Lua FCALL, per
KN1) — the real collision is with `suspend`'s own waitpoint-creation path.

**Required fix.** Either:
(a) Keep `create_pending_waitpoint` on the trait (matches SDK + Lua, verbose
    but unambiguous).
(b) Rename to `issue_pending_waitpoint` (distinguishes "issue a token for
    future delivery" from "suspend and wait on signals").
(c) Keep `create_waitpoint` but document IN THE METHOD DOCSTRING (v2 lines
    146-149 gesture at it, but don't spell out the pending/active contrast)
    that the method creates the pending-kind and that `suspend` creates the
    active-kind — plus rename the wire-level Lua error arm references in the
    RFC prose to match the trait.

v2's current rationale (line 169) is factually wrong about "pending" being
Valkey-internal; fix the rationale or fix the name.

---

## DESERVES-DEBATE

### #LD1: Trait `suspend` method becomes USED-BY-NOBODY after landing — a breaking trait signature change with zero runtime exercise

v2 §R7.2.2 line 136 ("ff-backend-valkey::suspend remains Unavailable-stubbed
after this PR") + §R7.3 line 235 ("suspend site stays direct-FCALL until
Stage 1d") + §R7.7 acceptance-gate (1) excluding suspend ⇒ after the amendment
lands:

- Trait signature changes (`Handle` → `SuspendOutcome`).
- ff-backend-valkey impl: returns `Err(EngineError::Unavailable)`.
- SDK `ClaimedTask::suspend`: bypasses the trait entirely (direct-FCALL).
- NO production caller exercises `EngineBackend::suspend`'s new return type.

Evidence the Unavailable stub will survive: `crates/ff-backend-valkey/src/lib.rs:1173-1180`
returns Unavailable today; per v2 line 234 "`suspend` return type ONLY (KD1:
body stays `Unavailable` until input-shape work lands)."

**Debate question.** Is landing a breaking trait signature change whose real
body is stubbed and whose real caller bypasses the trait acceptable? Three
framings:

1. *Yes — shape-reservation is the purpose.* Locks in the return-type contract
   now so Stage 1d's input-shape work is purely additive on the signature
   side. OK if we accept that CI will not catch regressions on the shape.

2. *No — co-land the input-shape work.* Amendment grows to cover §R7.6.1.
   Heavier, but the trait method is actually exercised on merge.

3. *No — defer `suspend` from this amendment.* Land the 3 clean cases now
   (`append_frame`, `report_usage`, `create_waitpoint`). `suspend`'s typed
   outcome ships with Stage 1d. Amendment's scope is more honest
   ("3 deferrals resolved, 1 remaining") and matches what actually ships.

v2 chose (1) implicitly but didn't name the tradeoff. Owner needs to endorse.

---

### #LD2: Envelope creep framing — "15 in spirit" → 17 `async fn` is a real jump, and `create_waitpoint` is NOT a "splits-for-honesty" move

RFC-012 §3.1 line 109 justifies the 13→15 envelope stretch as:
"every count-growth step is a splits-for-honesty move, not a granularity creep
(the `progress`/`append_frame` split replaces a fused method whose atomicity
was unsound; `observe_signals`/`claim_from_reclaim` replaces one method whose
multi-round-trip honesty was broken; `delay` and `wait_children` elevate two
call sites that are structural peers of `suspend`)."

v2's open-question #5 (line 311) flags the envelope concern but doesn't resolve
it. Crucial distinction K didn't probe:

**`create_waitpoint` is a net-new business op, not a split.** Every prior
growth step was: one op that was dishonestly fused → split into honest
siblings. Method count grows but the business-op count was unchanged
(or shrank). Adding `create_waitpoint` grows both method count AND business-op
count. It's a precedent shift on the envelope framing.

**Debate question.** Does the "15 in spirit" envelope language in §3.1 need
explicit relaxation in this amendment's RFC text (e.g. amending §3.1 to read
"~10-12, up to ~17, with splits-for-honesty AND gap-fills"), or is the
envelope's "in spirit" qualifier elastic enough as-stated? If the former,
§R7.3 needs a bullet for RFC-012 §3.1 text update.

---

### #LD3: `AppendFrameOutcome` derive posture drift

v2 §R7.2.1 line 72 widens `AppendFrameOutcome` from SDK's `Clone, Debug` to
`Clone, Debug, PartialEq, Eq`. Non-breaking (both fields impl `PartialEq+Eq`).
Soft concern: v2 pulls this type out of the SDK's minimal-derive posture to
match `FailOutcome`, and `PendingWaitpoint` follows the same expanded posture.
Document the new canonical posture in §R7.4 or §R7.5 to avoid silent drift.

---

## NITS

### #LN1: v2's §R7.3 bullet 5 "update 4 signatures" miscounts

v2 line 238: "update 4 signatures, add `create_waitpoint`".

Amendment changes 3 signatures (`append_frame`, `suspend`, `report_usage`) and
adds 1 (`create_waitpoint`). The 4th "signature" v2 is counting is
`create_waitpoint` itself, which is ALSO the "add" item. Either "update 3
signatures, add `create_waitpoint`" or "touch 4 trait methods (3 updates, 1
add)" — pick one to not double-count.

### #LN2: v2 line 17 grep count attribution

v2 line 17 claims `grep -c "^    async fn " crates/ff-core/src/engine_backend.rs`
returns 16. Verified empirically (16 matches at `da89fa9`). Good. But the
related claim "RFC §3.1 taxonomy (which counts `append_frame` as '3b')
separately reads 16" — RFC §3.1 line 107 reads "15 methods — round-5 amended",
not 16. The 3b numbering makes the count 15 in the RFC-taxonomy, not 16.
v2's parenthetical is off by one. Fix to: "RFC §3.1 taxonomy reads 15 (counts
`append_frame` as '3b'); `async fn` compile count reads 16; post-amendment:
16 / 17 respectively."

### #LN3: §R7.6.5's dyn-safety smoke test recommendation should be elevated

v2 line 309 recommends "yes" on the smoke test but leaves it as a question.
§R7.7 acceptance-gate (3) already requires `_assert_dyn_compatible` + `_dyn_compatible`
compile cleanly — those are static assertions. The smoke test is stricter
(exercises dispatch). Either include it in gate (3) or drop from the open-
question list. Currently inconsistent.

### #LN4: v2 line 314 "Challenger discipline. K-round applied (v1 → v2). Next: L / M / P" is presumptuous

L/M/P is a four-round ritual from the namespace amendment. This amendment may
or may not need all four. Suggest: "Next: L (this document), then M/P if
material findings remain."

---

## Affirmative no-findings (including K's findings correctly addressed)

- **K1 (`ReportUsageResult` `#[non_exhaustive]` landing):** v2 §R7.2.4 lines
  213-215 land the attribute at amendment time, §R7.3 bullet 4 lists it as a
  consumer-update, §R7.5.1 no longer hand-waves. Resolved. **Downstream-match
  audit**: grep for `ReportUsageResult::` across crates finds no cross-crate
  exhaustive match without a wildcard (ff-sdk `task.rs:2157,2170` use
  `other => panic!()`; ff-test uses `matches!`; ff-script / ff-server construct,
  do not match). Adding `#[non_exhaustive]` breaks nothing in-tree. Clean.
- **K2 (`suspension_id` on both variants):** Present on both variants (v2
  lines 111, 120). See #L1 for the ORTHOGONAL concern about `handle` — K2
  itself is cleanly addressed.
- **K3 (`AppendFrameOutcome` derive alignment with `FailOutcome`):** v2
  line 65 matches FailOutcome precedent (`Clone, Debug, PartialEq, Eq`, NO
  `#[non_exhaustive]`). Divergence-from-v1 spelled out at line 72. Clean.
  See #LD3 for a soft consistency question.
- **K4 (16/17 method-count framing):** v2 §R7.2.3 line 173 names both
  framings explicitly (17 `async fn` compile-count; 16 RFC-taxonomy). See
  #LN2 for a one-word fix in the parenthetical.
- **K5 (`append_frame` stub reality):** v2 §R7.2.1 line 78 correctly reports
  `Err(EngineError::Unavailable { op: "append_frame" })`. Fixed.
- **KD1 (suspend input-shape scope-out):** v2 §R7.7 acceptance-gate (1)
  excludes suspend explicitly, §R7.2.2 line 136 owns the "Unavailable-stubbed
  after this PR" consequence, Stage 1d tracker at §R7.6.1. Resolved — though
  see #LD1 for the shape-reservation tradeoff owner should endorse.
- **KD2 (cairn grep-across-peer-trees):** v2 §R7.3 line 230 rephrased to
  "no direct `EngineBackend` impls outside `ff-backend-valkey`" (in-tree
  verifiable). Cross-repo grep claim dropped. Clean.
- **KD3 (`CheckAdmissionResult` corroboration):** v2 §R7.5.1 line 257 cites
  `contracts.rs:1447-1457`. Verified — type exists with the variant set K
  described. Clean.
- **KD4 (§3.1.1 sub-bucket):** v2 §R7.6.2 line 277 accepts "keep in §3.1.1,
  no new sub-bucket." Clean.
- **KN1 (collision framing):** v2 line 171 weakened to "optics; `create_`
  stays the neutral verb." Clean at the Lua-vs-trait distinction. See #L2
  for a DIFFERENT collision concern.
- **KN2 (`WaitpointHmac` no Display):** v2 line 167 states "no Display impl
  (KN2 fix — v1 draft misstated this)." Verified at `backend.rs:273-277` —
  only `impl Debug` exists. Clean.
- **KN3 (`AlreadySatisfied` reframe):** v2 line 132 "trait growth to match
  existing wire." Verified at `crates/ff-script/src/functions/suspension.rs:83-84`
  (wire emits `ALREADY_SATISFIED` today). Clean.
- **KN4 (`Duration` resolution):** v2 §R7.6.4 line 281 resolved to Duration.
  Clean.
- **Dyn-safety posture:** Proposed shapes use concrete types only. Static
  assertions will re-verify. See #LN3 for the smoke-test promotion.
- **`ReportUsageResult` non_exhaustive landing does NOT break in-tree
  matches:** Verified via exhaustive grep of all `match` and `matches!` sites
  across `crates/`; every cross-crate match either uses a wildcard arm or
  `matches!`-with-single-variant. In-crate (same-crate-as-definition) matches
  are unaffected by `#[non_exhaustive]`. Safe.

---

## Verdict

**Accept with changes.** v2 cleanly resolved all five of K's must-fixes and
eight of K's debate/nit items. Two fresh must-fixes surfaced — #L1 (phantom
`handle` field on `SuspendOutcome::Suspended`) is the load-bearing one: v2's
"matching SDK" claim is factually wrong and papers over a genuine data-model
decision the owner needs to make. #L2 (pending vs create rename) is a
docstring-or-rename fix. #LD1 and #LD2 deserve owner adjudication before
landing. Remaining items are editorial. Structural case still sound.
