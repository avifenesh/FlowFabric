# RFC-014 Round 2 — author response

L: ACCEPT. K and M: DISSENT on small doc-consistency gaps introduced by
round-1 edits (two contradictions + two cross-reference drifts). All
concede-worthy.

## Disposition

| Finding | Disposition |
|---|---|
| K-R2-1 §4.4 JSON example missing source_type breakdown | CONCEDE — JSON example updated to show `sources_by_type: { "user": 1, "system": 1 }` on the Count node; explanatory line added. |
| K-R2-2 `appended_to_waitpoint_duplicate` missing from §5.2 | CONCEDE — added as a new row in §5.2 with the AllOf-re-fire case explicit. |
| K-R2-3 §6.1 table / paragraph contradiction on `auto_resume_with_timeout_signal` | CONCEDE — rewrote the table row to match the round-1 paragraph (unconditional node-satisfier short-circuit; no "consumer opt-in via matcher"). The table and the paragraph now agree. |
| L non-blocking: `CountKind` default | CONCEDE (drive-by) — §10.3 now specifies `DistinctWaitpoints` as the default when no kind selector is called, with rationale. |
| M-R2-1 Phase-1 exit enum shape drift | CONCEDE — Phase-1 bullet now says `InvalidCondition { kind, detail }` per §5.1.1. |
| M-R2-2 Phase-4 tracing bullet missing `signal_ignored_matcher_failed` | CONCEDE — added to the tracing bullet. |

## No arguments back

All six findings are consistency drift from round-1 edits. Nothing to
defend.

## Changes to core design

None. All edits are §4.4 example, §5.2 table, §6.1 table, §10.1 exit
bullet, §10.3 default-kind note, §10.4 tracing bullet. Text-only.
