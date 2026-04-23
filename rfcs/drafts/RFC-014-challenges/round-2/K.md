# RFC-014 Round 2 — K (correctness) challenge

**Verdict:** DISSENT (two residual correctness inconsistencies introduced
by round-1 edits; both small, both blocking).

Round-1 revisions addressed all six of my correctness findings. I audited
the revised RFC top-to-bottom for (a) regressions introduced by the
edits and (b) anything I missed the first time. Two real issues.

## Per-section verdict

| Section | Signal | Notes |
|---|---|---|
| §0 Forward-compat | GREEN | Unchanged, still good. |
| §1.1–1.4 | GREEN | §1.4 canonical examples close L-1; correctness-wise clean. |
| §2 Enum | GREEN | |
| §3.1 + 3.1.1 + 3.1.2 | GREEN | Cleanup owners + key budget closed K-2 / M-1. |
| §3.2 | GREEN | Source-type-in-token closed K-3; leaf-token paragraph closed K-4. |
| §3.3 | GREEN | Step 2.5 closed K-1; `is_single_leaf` guard closed K-4. |
| §4.1–4.2 | GREEN | |
| §4.3 | GREEN | |
| §4.4 | YELLOW | Narrative says output "includes per-DistinctSources source_type breakdowns" but the JSON example at §4.4 lines 365–373 doesn't show one. Consumer implementing against the example will miss it. See K-R2-1. |
| §4.5 | GREEN | Closed K-5. |
| §5.1 + 5.1.1 | GREEN | `detail: String` closed L-3. |
| §5.2 | YELLOW | Missing entry for `appended_to_waitpoint_duplicate`. It's referenced in §3.3 pseudocode and §4.2 but not in the §5.2 effect table. See K-R2-2. |
| §5.3–5.5 | GREEN | |
| §6.1 | **RED** | The `timeout_behavior` table row for `auto_resume_with_timeout_signal` (line 493) says "If the condition treats the timeout token as satisfying (consumer opt-in via matcher, §6.2), resumes. Otherwise falls through to fail." The paragraph that follows (lines 500–514, the round-1 addition) contradicts this: "A `Count { n, kind }` node that sees the timeout token short-circuits to satisfied ... it does NOT increment the distinct-satisfier count. ... The short-circuit fires only when `timeout_behavior == auto_resume_with_timeout_signal`." Either it's consumer-opt-in-via-matcher OR unconditional-node-short-circuit — can't be both. A reader will not know which is authoritative. See K-R2-3. |
| §6.2–6.3 | GREEN | |
| §7–10 | GREEN | |

## Residual concerns

### K-R2-1 — §4.4 JSON example missing source_type breakdown

The text after the JSON block says source_type breakdowns are included.
The JSON block doesn't show one. Consumers implement against examples
before they read surrounding prose.

**Minimal fix:** update the JSON example in §4.4 to show a
`DistinctSources` node with the breakdown field:

```json
{ "path": "members[0]", "kind": "Count(2-of-3, DistinctSources)",
  "satisfied": false, "sources_by_type": { "user": 1, "system": 0 } }
```

### K-R2-2 — `appended_to_waitpoint_duplicate` missing from §5.2

Round-1's §3.3 pseudocode returns `appended_to_waitpoint_duplicate` when
SADD returns 0 (token already present). §4.2 references it by name. §5.2
("Late-detection at signal delivery") does NOT list it. Either it's an
effect or it's an error — and the RFC text treats it as an effect, but
the §5.2 table is the authoritative effect enumeration.

**Minimal fix:** add a row to §5.2:

| `appended_to_waitpoint_duplicate` (existing, clarified by RFC-014) | Signal matches a satisfier token already in `satisfied_set`. Signal is recorded on the waitpoint's signal list; no re-evaluation. Pre-existing RFC-005 effect extended to token-level dedup per §4.1. |

### K-R2-3 — §6.1 table row contradicts the round-1 clarification

Line 493 (table): `auto_resume_with_timeout_signal` → "If the condition
treats the timeout token as satisfying (consumer opt-in via matcher,
§6.2), resumes. Otherwise falls through to fail."

Lines 500–514 (round-1 addition): "A `Count { n, kind }` node that sees
the timeout token short-circuits to satisfied ... The short-circuit
fires only when `timeout_behavior == auto_resume_with_timeout_signal`."

The table says "opt-in via matcher." The paragraph says "unconditional
short-circuit when the behavior is set." These are incompatible
descriptions of the same thing.

Correct semantics (inferable from §6.2–6.3 which reject a `PartialCount`
variant and push application-level logic to the worker): the
`auto_resume_with_timeout_signal` behavior unconditionally resumes with
the synthetic timeout token added; the worker decides on the resumed
side. There is no "consumer opt-in via matcher" — the matcher is on the
`Count`, and a timeout token has no source_identity for the matcher to
test.

**Minimal fix:** rewrite the §6.1 table row for
`auto_resume_with_timeout_signal`:

| `auto_resume_with_timeout_signal` | Synthetic timeout token `timeout:<suspension_id>` added to `satisfied_set`. Root condition is treated as satisfied (the timeout token is a universal node-satisfier; see paragraph below). Execution resumes. The worker inspects `list_waitpoint_signals` and `all_satisfier_signals` (§4.5) to branch on "was this timeout-induced or full-quorum" per §6.2. |

This matches the round-1 paragraph and removes the contradiction.

## Flip-to-ACCEPT requirements

- K-R2-1: update §4.4 JSON example to show source_type breakdown.
- K-R2-2: add `appended_to_waitpoint_duplicate` row to §5.2.
- K-R2-3: rewrite `auto_resume_with_timeout_signal` row in §6.1 table
  to match the round-1 clarification; drop the "consumer opt-in via
  matcher" phrasing.

All three are doc consistency fixes. No design change.
