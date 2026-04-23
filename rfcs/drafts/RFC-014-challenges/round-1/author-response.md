# RFC-014 Round 1 — author response

Three DISSENTs (K, L, M). All findings are concede-worthy; none of them
dispute the core enum shape, storage model, or matching algorithm. The
revisions are either (a) spelling out invariants the author held in head
but not on page, or (b) minor ergonomic surface additions.

## Disposition

| Finding | Disposition | Location |
|---|---|---|
| K-1 matcher scope under nesting | CONCEDE — added step 2.5 to §3.3 pseudocode; added `signal_ignored_matcher_failed` to §5.2. |
| K-2 orphan cleanup owners | CONCEDE — added §3.1.1 naming three Lua sites; added `expire_` and `cancel_delete_...` integration tests to §10.2. |
| K-3 DistinctSources Q1 | CONCEDE — closed Q1 in §8 as "system sources count; source_type is part of token"; moved rationale into §3.2; added `count_2_distinct_sources_counts_system_signal` + `count_matcher_filters_user_only_sources` tests. No new `DistinctUserSources` variant (matcher-based filtering is enough). |
| K-4 leaf token shape | CONCEDE — restated `evaluate_node` rule explicitly with `m is Single` vs non-`Single` branches; added `is_single_leaf` guard in §3.3 step 4 so leaf `Single`s don't emit redundant `node:` tokens. |
| K-5 resume payload | CONCEDE — added §4.5 with `closer_signal_id` + `all_satisfier_signals`; added `resume_payload_exposes_...` test. |
| K-6 timeout token form | CONCEDE — specified token is `timeout:<suspension_id>`, treated as a node-level short-circuit (not a distinct-count increment); added `timeout_token_short_circuits_count_node` test. |
| L-1 worked examples | CONCEDE — added §1.4 with concrete builder-form for all three §1.1 patterns. |
| L-2 canonical 2-of-5 style | CONCEDE — §1.4 picks shared waitpoint + `DistinctSources` as canonical for N-of-M-reviewers; multi-waitpoint is canonical for heterogeneous-subsystem pattern 3. |
| L-3 error detail field | CONCEDE — added §5.1.1 with `EngineError::InvalidCondition { kind, detail: String }`. |
| L-4 timeout-partial idiom | CONCEDE — §6.2 now shows the `SuspensionTimedOut { partial_satisfiers }` match idiom; no new variant, additive error field. |
| L-5 builder API fully specified | CONCEDE — §10.3 now enumerates the full `CountBuilder` + `ResumeCondition` builder surface including `all_of_waitpoints` shorthand and `on_waitpoint`/`on_waitpoints` split. |
| M-1 key budget | CONCEDE — added §3.1.2 naming the before/after key set and hash-tag co-location. |
| M-2 parse-scope clarity | CONCEDE — §3.3 first line now explicit: "per-invocation only, no cross-call cache." |
| M-3 cap rationale | CONCEDE — added §5.5 justifying depth 4 and size 8 KiB with headroom math and "both caps are soft" note. |
| M-4 cluster co-location tests | CONCEDE — added two cluster-mode tests in `crates/ff-backend-valkey/tests/cluster/`. |

## No arguments back

Nothing to push back on in round-1. All findings improve the RFC without
compromising the core design. If reviewers want to argue against the
shape (the enum extension vs parallel type, the SET+HASH vs nested
storage, the node-level timeout short-circuit vs a new `PartialCount`
variant), that fight belongs in round 2 — but §9 already argues against
each alternative and none of K/L/M disputed those.

## Changes to core design

None. The enum shape (§2.1), storage model (§3.1), matching algorithm
(§3.3), and timeout composition (§6) are unchanged in structure.
Everything else is clarifications, rationale, cleanup owners, tests, and
builder surface.
