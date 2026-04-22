# Round-1 CHALLENGE against RFC-012 amendment #117 deferrals (draft v1)

Reviewer: Worker K. Base: `da89fa9` (matches draft). Read-only.

## MUST-FIX

### #K1: `ReportUsageResult` is NOT `#[non_exhaustive]` — the §R7.2.4 and §R7.5.1 YAGNI argument is built on a false premise

Draft §R7.2.4 (line 200): "`#[non_exhaustive]` stays on the enum so future
`Throttled` / `Rejected` additions are additive."
Draft §R7.5.1 (line 231): "`#[non_exhaustive]` makes those additive later."

**Evidence.** `crates/ff-core/src/contracts.rs:1400-1401`:
```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportUsageResult {
```
No `#[non_exhaustive]`. Grep confirms — the only `non_exhaustive` attributes in
the file are on admin-surface args, not on `ReportUsageResult`.

**Why it's load-bearing.** The draft's replace-over-widen case rests on
"speculative variants can come back later because `#[non_exhaustive]`." Without
the attribute, adding `Throttled` / `Rejected` post-replace is itself a
breaking change on `ReportUsageResult`, re-opening exactly the cost the replace
was supposed to amortise.

**Required fix.** Either (a) the amendment's trait edits MUST include
`#[non_exhaustive]` on `ReportUsageResult` at landing time (add to §R7.3's
consumer-update list and spell it out in the trait-delta section), or (b) the
replace-vs-widen argument needs a different justification (e.g. "we accept
that Throttled will be a second breaking change if it ever lands"). Option (a)
is strictly better; pick it and flag that the ff-core type grows an attribute.

---

### #K2: `SuspendOutcome` proposed shape contradicts the same section's design note — `suspension_id` is in the prose, missing from the enum

Draft §R7.2.2 lines 86-103 (proposed enum) lists variant fields:
- `Suspended { handle, waitpoint_id, waitpoint_key, waitpoint_token }`
- `AlreadySatisfied { waitpoint_id, waitpoint_key, waitpoint_token }`

Draft §R7.2.2 line 106: "Preserve it on the trait type. It's caller-visible
for observability and is already on the wire."

**Evidence that the SDK variants DO carry `suspension_id`.**
`crates/ff-sdk/src/task.rs:71-90`:
```
pub enum SuspendOutcome {
    Suspended { suspension_id: SuspensionId, waitpoint_id, waitpoint_key, waitpoint_token },
    AlreadySatisfied { suspension_id: SuspensionId, waitpoint_id, waitpoint_key, waitpoint_token },
}
```

SDK construction at `crates/ff-sdk/src/task.rs:1550-1562` wires `suspension_id`
into both variants. Every consumer that currently matches (examples,
ff-readiness-tests, resume_signals_integration.rs) can ask for it.

**Required fix.** Either add `suspension_id: SuspensionId` to both variants in
the proposed shape (match §R7.6.3's lean toward keeping it), OR remove the
design note on line 106 so the draft is internally consistent. The two cannot
both stand.

---

### #K3: Moving `AppendFrameOutcome` to `ff-core::backend` is NOT "field shape unchanged" — derive set changes and `#[non_exhaustive]` is net-new

Draft §R7.2.1 line 43: "Field shape unchanged."

**Evidence — current shape.** `crates/ff-sdk/src/task.rs:137-143`:
```
#[derive(Clone, Debug)]
pub struct AppendFrameOutcome { pub stream_id: String, pub frame_count: u64 }
```

**Draft proposed shape** (lines 45-51):
```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AppendFrameOutcome { pub stream_id: String, pub frame_count: u64 }
```

Adds `#[non_exhaustive]`. Drops no derives, but the FailOutcome precedent
(`crates/ff-core/src/backend.rs:546-551`) ships with `PartialEq, Eq` for
test-asserting consumers and deliberately NOT non_exhaustive (the
in-file comment is explicit: "Not `#[non_exhaustive]` because existing
consumers ... construct and match this enum exhaustively"). The draft invokes
the FailOutcome precedent to justify the move but then breaks the precedent's
two deliberate choices without discussion.

**Required fix.** Either (a) match FailOutcome (no `non_exhaustive`, add
`PartialEq, Eq` for ergonomics), or (b) keep `non_exhaustive` and explicitly
document why this type chooses differently from FailOutcome. Don't silently
diverge.

---

### #K4: Trait method count is off by one — amendment lands at 17 `async fn`, not 16

Draft §R7.2.3 line 150: "Updated inventory count: **16 methods** (was 15 post
round-5)."

**Evidence.** `grep -c "^\s*async fn " crates/ff-core/src/engine_backend.rs`
returns **16** today. The RFC §3.1 headline reads "15 methods" because it
counts `append_frame` as method "3b" (a sub-number of `progress`). By
`async fn` count, trait is already at 16. Adding `create_waitpoint` = 17.

Draft can keep the "16 methods" RFC-taxonomy framing if it names it
consistently (3b stays 3b), but §R7.6.2 also flags the sub-bucket question.
Pick one: either own "17 `async fn`" publicly with a CHANGELOG-friendly
sentence, or explicitly clarify the numbering convention matches §3.1's
"3b"-style counting.

**Why this matters.** §R7.6.2 asks the owner whether a `§3.1.4` sub-bucket is
needed. If the owner reads "16 methods" and thinks "same as today's 16," the
question is mis-framed. The real move is 16 → 17 concrete trait methods.

---

### #K5: Factual error — `ff-backend-valkey::append_frame` returns `Unavailable`, not `Ok(())`

Draft §R7.2.1 line 57: "`ff-backend-valkey::append_frame`; currently returns
`Ok(())`".

**Evidence.** `crates/ff-backend-valkey/src/lib.rs:1145-1147`:
```
async fn append_frame(&self, _handle: &Handle, _frame: Frame) -> Result<(), EngineError> {
    Err(EngineError::Unavailable { op: "append_frame" })
}
```

The same is true of `suspend` (`:1173-1180`) — all four Stage-1b deferrals
are stubbed `Unavailable`, not `Ok(())` / silent success. This is only a
factual error, not a structural one, but it undermines the draft's
"Breakage scope" credibility. Fix to match reality. (Note: draft correctly
reports the `report_usage` stub as `Unavailable` at §R7.1 line 21, so the
sloppy phrasing is isolated to §R7.2.1 line 57.)

---

## DESERVES-DEBATE

### #KD1: Scoping-out the `suspend` input mapping leaves the trait method still structurally unimplementable

Draft §R7.2.2 line 112 / §R7.4 line 219 / §R7.6.1 scope the
`(ConditionMatcher, TimeoutBehavior) → (Vec<WaitpointSpec>, Option<Duration>)`
mapping to Stage 1d.

**Evidence trait is currently `Unavailable` for a reason.**
`crates/ff-backend-valkey/src/lib.rs:1173-1180` — `suspend` is stubbed because
the SDK call site at `crates/ff-sdk/src/task.rs:812-818` cannot construct
`Vec<WaitpointSpec>` from its caller-supplied `&[ConditionMatcher]` +
`TimeoutBehavior` without going through the inline resume-condition-JSON
builder at `:836-850`. The SDK today builds the JSON payload *itself* and
passes it as ARGV, bypassing the `WaitpointSpec` shape.

**Debate question.** If round-7 lands the return-type widen but the input
shape is still unworkable, the `ff-backend-valkey::suspend` impl will stay
`Unavailable`-stubbed through Stage 1c. That means the SDK forwarder collapse
claimed in §R7.3.2 cannot actually happen for `suspend` — the SDK will still
`client.fcall(...)` directly, and the Stage-1d hot-path migration accrues the
cost the amendment promised to discharge.

**Possible resolutions.**
1. Acknowledge in §R7.7 that `suspend` is return-type-only and the SDK
   forwarder collapse for `suspend` specifically blocks on Stage 1d — i.e. one
   of the four deferrals stays half-deferred.
2. Expand the amendment to co-specify the input shape.
3. Land return-type only and accept that §R7.7's acceptance gate (1) "no
   remaining `client.fcall("ff_*", ...)` in `ClaimedTask`" cannot be met by
   this PR.

Pick one and say so. The draft currently implies all four lift cleanly, but
suspend won't.

---

### #KD2: Draft asserts cairn has zero consumers but cairn tree is not accessible in this checkout

Draft §R7.2.4 line 190: "Grep confirms no external consumers."
Draft §R7.3 line 203: "Cairn-fabric does not consume `EngineBackend` directly
today (they consume `ClaimedTask` via the SDK)."

**Evidence — non-dismissive.** `find / -maxdepth 4 -name cairn-fabric`
returns empty in this worktree. The draft's grep was run against `crates/` +
`ferriskey/`. If cairn is a peer-team tree not present locally, the "zero
consumers" claim is untested against cairn. The claim may be correct (cairn
depending on `ClaimedTask` not `EngineBackend` is consistent with RFC-012
posture), but the draft implies a grep-proof when it's actually an
architectural inference.

**Debate question.** Is the grep-across-peer-trees check part of the round-7
acceptance gate, or does the "cairn uses SDK not trait" architectural fact
suffice? The answer is probably the latter (consumers hit the SDK pub-use
shims, not the trait directly), but the draft should say so rather than
invoking a grep it can't reproduce in this checkout.

**Suggested fix.** Replace §R7.3's "per grep across `crates/` and
`ferriskey/`" with "peer consumers route through `ClaimedTask` (the SDK
forwarders stay shape-stable; `AppendFrameOutcome` / `SuspendOutcome` keep
`pub use` shims); grep in the FlowFabric repo confirms no direct
`EngineBackend` impls outside `ff-backend-valkey`."

---

### #KD3: `CheckAdmissionResult` already exists as the admission-rails type — strengthens replace-over-widen but draft doesn't cite it

`crates/ff-core/src/contracts.rs:1447-1457` defines
`CheckAdmissionResult::{Admitted, AlreadyAdmitted, RateExceeded, ConcurrencyExceeded}`.
This is exactly the rate-limit / policy-reject shape the draft's §R7.5.1
gestures at as "a hypothetical future rate-limit backend."

**Why it matters.** The draft rejects widen on the grounds that `Throttled` /
`Rejected` are speculative. But the admission-rails op already exists as a
separate type — future rate-limit logic plugs into `check_admission_and_record`
paths, not `report_usage`. This is STRONGER evidence for the replace than the
draft gives itself. Worth citing as corroboration in §R7.5.1: "the
admission-gating role already has its own type (`CheckAdmissionResult`)
and is conceptually a distinct op, not a variant of usage-reporting."

This is a debate item, not a must-fix — the draft reaches the right verdict,
just under-cites.

---

### #KD4: `§3.1.4 waitpoint-management` sub-bucket — no, don't add it

§R7.6.2 asks whether to split §3.1.1 into a new "waitpoint management"
sub-bucket.

**Pushback.** The §3.1 taxonomy is "Claim + lifecycle" vs "Read-path" vs
"Out-of-band." `create_waitpoint` + `suspend` + `observe_signals` all live on
the lease-held path — they are lifecycle ops. A fourth bucket for one new
method is optics not structure; the draft itself observes the new method
"is created on the lease-held path" (line 150) which IS §3.1.1. Keep it in
§3.1.1. Owner can veto.

---

## NITS

### #KN1: "create_waitpoint" naming — collision claim is Lua-scoped not trait-scoped

§R7.2.3 cites collision with `issue_claim_grant` which is a Lua FCALL name
(`crates/ff-script/src/functions/scheduling.rs:43`), NOT a trait method. The
only trait peer is `claim_from_reclaim` (`engine_backend.rs:144`). Verdict
(`create_waitpoint`) is fine; weaken the collision framing.

### #KN2: `WaitpointHmac` has no Display impl — Debug redacts, Display is absent

Draft §R7.2.3 line 143 says "redacts on Debug/Display."
`crates/ff-core/src/backend.rs:273-277` defines only `impl Debug`. No Display
impl exists on `WaitpointHmac`. The safety property holds (nothing to leak
through), but phrase as "has no Display; Debug redacts via the wrapped
`WaitpointToken` at `types.rs:570-586`."

### #KN3: `SuspendOutcome::AlreadySatisfied` is NOT net-new behaviour

§R7.2.2 line 108 calls it "net-new trait surface." The Lua wire contract
(`task.rs:1549-1555`) and SDK shape (`task.rs:84-89`) have carried it since
pending-waitpoint work landed. Rephrase as "trait grows to match existing
wire contract."

### #KN4: §R7.6.4 resolve in favour of `Duration` — no open question

`Duration` matches `suspend(timeout: Option<Duration>)`. SDK's `u64 ms`
(`task.rs:689`) is a compat artefact. Resolve, don't leave open.

---

## Affirmative no-findings

- **#K8 investigation (suspend `AlreadySatisfied` as "widening behaviour
  contract").** NOT a finding. The Lua contract at
  `ff_suspend_execution` already emits `ALREADY_SATISFIED`; the SDK already
  returns it. Draft correctly exposes existing behaviour; see #KN3 for the
  phrasing tweak.
- **#K7 investigation (HMAC redaction).** Shape-correct; see #KN2 for the
  Display nit. The structural guarantee holds.
- **#K1 investigation (zero external consumers of `AdmissionDecision`).**
  Grep across `crates/` + RFC text returns only 4 in-tree sites (type def,
  trait sig, stub impl, unit tests), all self-contained. Draft's verdict is
  sound; see #KD2 for the cairn-verification phrasing fix.
- **§R7.5 alternatives.** All four alternatives (replace vs widen, suspend
  widen vs split, create_waitpoint new-method vs fold, append_frame widen vs
  two-op) are well-argued with correctly-cited rejections. No disagreement.
- **Dyn-safety treatment (§R7.6.5).** Correct that concrete types + no
  associated types + no `impl Trait` returns preserve object-safety;
  `_assert_dyn_compatible` at `engine_backend.rs:205-206` and
  `_dyn_compatible` at `ff-backend-valkey::lib.rs:1286` will re-verify at
  compile. The proposal to add an explicit smoke-test is good hygiene;
  accept.

---

## Verdict

**Accept with changes.** Structural case sound; findings correctable in-place.
Blockers before promotion: #K1, #K2, #K3, #K4, #KD1. #K5 is one-line. Rest editorial.
