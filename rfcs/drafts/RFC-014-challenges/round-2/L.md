# RFC-014 Round 2 — L (ergonomics) challenge

**Verdict:** ACCEPT (with one note that does not block).

All five round-1 ergonomic findings were addressed directly:

| Finding | Round-2 verdict |
|---|---|
| L-1 worked examples | §1.4 covers the three patterns concretely. Clear which is canonical. |
| L-2 canonical 2-of-5 style | §1.4 picks shared-waitpoint + `DistinctSources` unambiguously. |
| L-3 error detail | §5.1.1 adds `detail: String`. |
| L-4 timeout-partial idiom | §6.2 adds the `SuspensionTimedOut { partial_satisfiers }` match idiom. |
| L-5 builder surface | §10.3 enumerates the full `ResumeCondition` + `CountBuilder` API. |

## Per-section verdict

| Section | Signal |
|---|---|
| §1 including §1.4 | GREEN |
| §2 | GREEN |
| §3 | GREEN (ergonomically — internal) |
| §4 | GREEN |
| §5 | GREEN |
| §6 | GREEN (idiom documented) |
| §7 | GREEN |
| §8 | GREEN (all Qs closed) |
| §9 | GREEN |
| §10 | GREEN |

## One non-blocking note

**§10.3 `CountBuilder` has no compile-time guarantee that a `CountKind`
was chosen before `on_waitpoint(s)`.** Calling `ResumeCondition::count(2).on_waitpoint(wp)`
with no `.distinct_sources()` / `.distinct_signals()` / `.distinct_waitpoints()`
will either silently default or runtime-error. The RFC doesn't say
which. Typed-state builders could make this a compile error. Not a
blocker because: (a) the RFC is about condition semantics, not SDK
type-level proof; (b) a runtime default to `DistinctWaitpoints` matches
the existing `satisfier_token` fallback in §3.3 (`node.kind or
"DistinctWaitpoints"`); (c) doctests in Phase 3 will expose the default
behavior quickly.

If the author wants to address this as a drive-by: state in §10.3 that
`CountBuilder::on_waitpoint(s)` with no kind set defaults to
`DistinctWaitpoints` (single-waitpoint + DistinctWaitpoints = classic
single-signal satisfied-once semantics). Two lines of text.

I do not require this for ACCEPT.

## ACCEPT
