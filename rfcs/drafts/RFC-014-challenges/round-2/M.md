# RFC-014 Round 2 — M (implementation) challenge

**Verdict:** DISSENT (one small implementation-plan gap).

All four round-1 findings are addressed. One residual that surfaced when
I re-read the full plan top-to-bottom.

## Per-section verdict

| Section | Signal |
|---|---|
| §0–3 | GREEN |
| §3.1.2 key budget | GREEN — closed M-1. |
| §3.3 parse-scope | GREEN — closed M-2. |
| §5.5 cap rationale | GREEN — closed M-3. |
| §10.2 cluster tests | GREEN — closed M-4. |
| §4 | GREEN |
| §5 | GREEN |
| §6 | GREEN |
| §7 | GREEN |
| §8 | GREEN |
| §9 | GREEN |
| §10.1 Phase 1 | **YELLOW** | Phase-1 exit text at line 666 names `EngineError::InvalidCondition { kind }` without the `detail: String` field that §5.1.1 (round-1 addition) requires. Inconsistent. See M-R2-1. |
| §10.4 Phase 4 tracing bullet | YELLOW | The tracing bullet at line 751 lists `appended_to_waitpoint_duplicate` and `signal_ignored_not_in_condition` as the effects to trace, but `signal_ignored_matcher_failed` (added in round 1 §3.3 step 2.5 + §5.2) is omitted. Operator won't see matcher-rejection traces. See M-R2-2. |

## Residual concerns

### M-R2-1 — Phase-1 exit enum shape drifted from §5.1.1

Line 666:

> Add suspend-time validators (§5.1) returning `EngineError::InvalidCondition { kind }` variants.

§5.1.1 defines `InvalidCondition { kind, detail: String }`. Phase-1
implementer reading the exit bullet as spec will ship the one-field
form. Then Phase 3 doctests (which render `detail`) won't compile.

**Minimal fix:** update line 666 to match §5.1.1 exactly:

> Add suspend-time validators (§5.1) returning `EngineError::InvalidCondition { kind, detail }` per §5.1.1.

### M-R2-2 — Phase-4 tracing bullet omits `signal_ignored_matcher_failed`

Line 751:

> Tracing: per-signal log of `effect` including new `appended_to_waitpoint_duplicate` and `signal_ignored_not_in_condition`.

`signal_ignored_matcher_failed` (from round-1 §3.3 step 2.5) is a new
effect too, and it's specifically the one that operator-facing tooling
needs visibility on: a matcher-rejected signal looks "lost" otherwise.

**Minimal fix:** update line 751:

> Tracing: per-signal log of `effect` including new `appended_to_waitpoint_duplicate`, `signal_ignored_not_in_condition`, and `signal_ignored_matcher_failed`.

## Flip-to-ACCEPT requirements

- M-R2-1: align Phase-1 exit text with §5.1.1's `InvalidCondition { kind, detail }` shape.
- M-R2-2: add `signal_ignored_matcher_failed` to the Phase-4 tracing bullet.

Both are one-line edits. No design change.
