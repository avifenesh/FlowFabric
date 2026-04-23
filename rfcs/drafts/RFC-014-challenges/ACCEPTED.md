# RFC-014 — Unanimous ACCEPT after 3 rounds

**Status:** Unanimous ACCEPT from K (correctness), L (ergonomics), and
M (implementation) as of round 3. Ready for owner adjudication.

**Branch:** `rfc/014-multi-signal-resume`
**PR:** #210 (open, not merged)
**RFC:** `rfcs/drafts/RFC-014-multi-signal-resume.md`

## Round-by-round

| Round | K | L | M | Net design change |
|---|---|---|---|---|
| 1 | DISSENT (6 findings) | DISSENT (5 findings) | DISSENT (4 findings) | None — all findings were spec fill-ins, rationale, and ergonomic surface. |
| 2 | DISSENT (3 consistency drifts from round-1 edits) | ACCEPT | DISSENT (2 plan/body drifts) | None — doc consistency only. |
| 3 | ACCEPT | ACCEPT | ACCEPT | — |

## Material changes from original draft

### Added during debate

- **§1.4** — Canonical worked examples for the three §1.1 patterns,
  picking shared-waitpoint + `DistinctSources` as canonical for the
  2-of-5 reviewer shape.
- **§3.1.1** — Cleanup owners named explicitly (deliver/cancel/expire
  Lua sites); integration tests to catch owner drift.
- **§3.1.2** — Per-suspension key budget delta stated (before/after).
- **§3.2** — Source-type-in-token rationale + Q1 closure; leaf-token
  shape restated; node:<path> only for non-leaf children.
- **§3.3 step 2.5** — Node-local matcher filter in the algorithm;
  `is_single_leaf` guard to prevent redundant node-tokens for leaf
  `Single` under `AllOf`.
- **§3.3 header** — Parse is per-invocation only (Valkey Functions
  are stateless).
- **§4.4** — Operator-diagnostic JSON now includes `sources_by_type`
  breakdown for `DistinctSources` count nodes.
- **§4.5** — Resume payload: `closer_signal_id` + `all_satisfier_signals`.
- **§5.1.1** — `EngineError::InvalidCondition { kind, detail: String }`.
- **§5.2** — Two new effects (`signal_ignored_matcher_failed`,
  `appended_to_waitpoint_duplicate`).
- **§5.5** — Cap rationale (depth 4, size 8 KiB; both soft).
- **§6.1 table row for `auto_resume_with_timeout_signal`** — Rewritten
  to match the "timeout token is universal node-satisfier" paragraph
  (removed the "consumer opt-in via matcher" contradiction).
- **§6.1 "Timeout token form" paragraph** — `timeout:<suspension_id>`
  single token; node-level short-circuit, not a distinct-count
  increment.
- **§6.2** — `SuspensionTimedOut { partial_satisfiers }` idiom with
  worked `match` snippet.
- **§8** — All three open questions closed.
- **§10.2** — Integration test list expanded to cover Q1 closure,
  matcher-based filtering, cleanup owners, timeout token form, resume
  payload shape, and cluster co-location.
- **§10.3** — Full builder API enumerated, including `all_of_waitpoints`
  shorthand, `on_waitpoint`/`on_waitpoints` split, and
  `DistinctWaitpoints` default behavior.
- **§10.4** — Tracing bullet lists all three new effects.

### Unchanged (core design)

- The enum extension strategy (variants on RFC-013's `ResumeCondition`,
  not a parallel `MultiSignalCondition`).
- The three `CountKind` values (`DistinctWaitpoints` / `DistinctSignals`
  / `DistinctSources`) — no `DistinctUserSources` added; user-only
  quorum expressed via `Count.matcher`.
- The flat `satisfied_set` SET + write-once `member_map` HASH storage
  model (O(1) Valkey keys per suspension regardless of condition
  complexity).
- The O(depth ≤ 4) per-signal evaluation cost; no per-node SETs.
- No new `timeout_behavior` value; no new `PartialCount` variant.
- No new waitpoint-spec fields; topology stays at the `ResumeCondition`
  layer.

## Final SHA

`0d2a389` (round-2 revisions) — round-3 ACCEPT commit TBD by the
orchestrator close-out commit.

## Constraints respected

- RFC-013's outer `ResumeCondition` shape was never challenged —
  RFC-014 stays within the forward-compat contract stated in §0.
- All challenges cited concrete §N text.
- Revisions were committed + pushed per round.
- PR #210 remains open for owner adjudication.
