# RFC-014 Round 1 — L (ergonomics) challenge

**Verdict:** DISSENT

The design is expressive, but expressiveness ≠ ergonomic. The 2-of-5
reviewer pattern is the canonical cairn use case, and walking through it
end-to-end with this RFC surfaces three ergonomic gaps. Each has a minimal
fix.

## Per-section verdict

| Section | Signal | Notes |
|---|---|---|
| §0 Forward-compat | GREEN | |
| §1.1 Motivation patterns | YELLOW | Pattern 1 (2-of-5 reviewers) is listed but no worked example is given — see L-1. |
| §2.1 Enum shape | YELLOW | `waitpoints: Vec<WaitpointKey>` for 2-of-5 is repetitive — see L-2. |
| §2.2/2.3 Rationale | GREEN | |
| §3 Storage + algorithm | GREEN | Internal detail; consumer never sees it. |
| §4 Idempotency | GREEN | |
| §5 Errors | YELLOW | Error taxonomy is correct, but the *user-facing* message for `condition_depth_exceeded` doesn't tell the consumer what depth their condition was — see L-3. |
| §6 Timeouts | YELLOW | §6.2 "write it as two sequential suspensions" is a non-answer. See L-4. |
| §7 Wire | GREEN | |
| §8 Open questions | YELLOW | Q3 is an ergonomic question, not resolved. |
| §9 Alternatives | GREEN | |
| §10.3 SDK ergonomics | **RED** | The Phase 3 builder sketch is too thin — reviewing it against the cairn 2-of-5 pattern shows the shape forces consumers into verbose construction. See L-5. |

## Concerns with concrete fixes

### L-1 — Worked example missing

§1.1 lists patterns but doesn't show what code a consumer writes. For a
design RFC this is tolerable, but "can it be expressed clearly" is literally
my challenge question, and I cannot answer without the author's intended
surface. I need to see:

```rust
// cairn: 2-of-5 reviewer approval
let cond = ResumeCondition::count(2)
    .distinct_sources()
    .on_waitpoints([wp_review_alice, wp_review_bob, wp_review_carol,
                    wp_review_dave, wp_review_eve]);
```

vs.

```rust
// cairn: 2-of-5 reviewer approval, single shared waitpoint
let cond = ResumeCondition::count(2)
    .distinct_sources()
    .on_waitpoint(wp_reviewers);  // one waitpoint, 5 signals land on it
```

The RFC hints both are expressible but doesn't pick a canonical. Cairn
operators reading this RFC will not know which to reach for.

**Minimal fix:** add §1.4 "Canonical worked examples" covering the three
§1.1 patterns with concrete pseudo-Rust using the §10 Phase 3 builder.

### L-2 — Repetition in 2-of-5 with distinct waitpoints

If the team adopts the "one waitpoint per reviewer" style, the consumer
writes 5 `WaitpointSpec`s plus the `Count` list. Ergonomic pain point:
creating a waitpoint per reviewer is a per-execution fan-out that may not
even be known at suspend-time (reviewer list comes from policy).

**Minimal fix:** pick one style as canonical in §1.4 and recommend the
shared-waitpoint form for "N-of-M human reviewers where M comes from
policy." Explicitly note: single waitpoint + `DistinctSources` is the
preferred shape; multi-waitpoint + `DistinctWaitpoints` is for "heterogeneous
subsystems must each report" (pattern 3).

### L-3 — Error message fidelity

§5.1 lists error kinds but not the payload shape. For a consumer hitting
`condition_depth_exceeded`, "depth 5 > cap 4" is actionable;
"condition_depth_exceeded" alone is not.

**Minimal fix:** add §5.1.1:

```rust
EngineError::InvalidCondition {
    kind: ConditionErrorKind,
    detail: String,   // human-readable: "depth 5 exceeds cap 4 at path members[0].members[0].members[0]"
}
```

Trivial, but the detail field is what cairn's error-surfacing code actually
renders.

### L-4 — §6.2 punts ergonomics to retry-handler

Current §6.2: "if a consumer wants *condition-level* 'count(3) OR
timeout-partial,' they must write it as two sequential suspensions." This
is punt, not answer. The natural cairn pattern is "wait 30m for 2-of-5; if
only 1 arrived, escalate to manager; if 2+ arrived, proceed." Two sequential
suspensions means:

1. Suspend `Count(2-of-5) timeout=30m behavior=fail`.
2. Catch failure in worker code, inspect `list_waitpoint_signals`, decide.

This works but forces try/catch around `suspend` for what a consumer reads
as "main-path" logic.

**Minimal fix:** keep the design (no new variant — I agree with §6.3's
rejection) but document the idiom in §6.2 as:

```rust
match self.suspend(spec, cond_count_2_of_5, timeout_30m).await {
    Ok(resume) => handle_full_quorum(resume),
    Err(EngineError::SuspensionTimedOut { partial_satisfiers }) =>
        handle_partial(partial_satisfiers),
}
```

Requires `SuspensionTimedOut` to carry `partial_satisfiers` (tokens that
DID land). That's an additive ergonomic change that unblocks the pattern
without a new variant.

### L-5 — Phase 3 builder sketch is too thin

§10.3: "`ResumeCondition::all_of([...])`, `ResumeCondition::count(n).distinct_sources().on_waitpoints([...])`."

Review against the three canonical patterns from §1.1:

- Pattern 1 (2-of-5): builder works cleanly.
- Pattern 2 (N callbacks from webhook): `count(n).distinct_signals().on_waitpoint(wp)`? The sketch doesn't show the signal-mode variant.
- Pattern 3 (all-of distinct event types): `all_of([Single::of(wp1), Single::of(wp2), Single::of(wp3)])` is verbose. A shorthand `all_of_waitpoints([wp1, wp2, wp3])` saves ten characters × every consumer.

**Minimal fix:** §10.3 must enumerate the builder API explicitly, not
sketch it. At minimum:

```rust
impl ResumeCondition {
    pub fn single(wp: WaitpointKey, matcher: ConditionMatcher) -> Self;
    pub fn all_of(members: impl IntoIterator<Item = ResumeCondition>) -> Self;
    pub fn all_of_waitpoints(wps: impl IntoIterator<Item = WaitpointKey>) -> Self; // shorthand
    pub fn count(n: u32) -> CountBuilder;
}

impl CountBuilder {
    pub fn distinct_waitpoints(self) -> Self;
    pub fn distinct_signals(self) -> Self;
    pub fn distinct_sources(self) -> Self;
    pub fn with_matcher(self, m: ConditionMatcher) -> Self;
    pub fn on_waitpoints(self, wps: impl IntoIterator<Item = WaitpointKey>) -> ResumeCondition;
    pub fn on_waitpoint(self, wp: WaitpointKey) -> ResumeCondition; // shorthand for single
}
```

Doctests for the three §1.1 patterns each.

## Flip-to-ACCEPT requirements

- L-1 + L-5 merged: add §1.4 worked examples AND expand §10.3 with the
  builder surface. These are the same ergonomic gap and must be closed
  together.
- L-2: pick a canonical style for the 2-of-5 pattern.
- L-3: specify `detail: String` on the error.
- L-4: add the `SuspensionTimedOut { partial_satisfiers }` idiom to §6.2.
