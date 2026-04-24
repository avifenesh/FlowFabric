# RFC-017 — Debate record: ACCEPTED

**RFC:** `rfcs/drafts/RFC-017-ff-server-backend-abstraction.md`
**Branch / PR:** `rfc/017-consolidated` → PR #261
**Final polish commit:** `9a905170657896ff64181aac5f548d6d87d341d0`
**Date:** 2026-04-23

## Verdict

Both challengers (K: correctness + schema + distributed invariants; L: cairn operations + migration pragmatics) reached **ACCEPT** on round 2. The RFC is ready for owner adjudication.

## Round-1 summary

- **K — 12 findings:** 11 full CONCEDE + REVISE; 1 PARTIAL CONCEDE (F3 T/M/H — `get_execution_result` kept at M with recorded argue-back; other three rows upgraded). All other findings (F1 trait count, F2 `#[non_exhaustive]` discipline, F4 `report_usage_admin`, F5 PendingWaitpointInfo schema, F6 D7 invariant, F7 Retry-After wiring, F8 Server::scheduler retirement, F9 silent 501s → §9.0 hard-gate, F10 config ownership, F11 open-question pruning, F12 shutdown_prepare under load) resolved in the revised RFC.
- **L — 5 findings:** all operationally resolved. POSTGRES_PARITY_MATRIX.md at Stage A + Stage-D parity CI gate (L-1); §9.3 consumer migration cadence + trait-stability freeze (L-2); §8 functional-plus-warn → hard-remove for waitpoint_token (L-3); §16 per-op cross-backend semantics appendix (L-4); Q5 closed with peer-team commitment in §10 (L-5).

Round-1 author response: `rfcs/drafts/RFC-017-challenges/round-1/author-response.md` (with round-2 count-reconciliation annotation at the top).

## Round-2 summary

Both challengers flipped to ACCEPT with 7 non-blocking YELLOW clarifications — all surface-level polish, none required new design decisions. Applied in commit `9a905170657896ff64181aac5f548d6d87d341d0`:

| # | Source | Item | Resolution |
|---|---|---|---|
| 1 | K-R2-N1 | §9.0 fleet-wide cutover callout | Added paragraph: `BACKEND_STAGE_READY` is per-node; rolling Stage D→E with `FF_BACKEND=postgres` unsupported; fleet-wide cutover required; release notes + parity matrix header carry callout. |
| 2 | K-R2-N2 | §16 `cancel_flow` reframing | Rewrote two "leans" as two owner options pending Q2 per §13.6 anti-pattern (no asymmetric trait semantics). |
| 3 | K-R2-N3 | Q2 narrowing (advisory) | Added author's non-binding lean to §12 Q2: header-only on the trait + Postgres inherent transactional method per §13.6. Owner confirm still required; Q2 remains open. |
| 4 | L-R2-1 | §9.3 trait-stability freeze tightening | Additive `#[non_exhaustive]` field growth only during B-D; non-additive changes deferred to post-Stage-E (v0.9.0+); cairn mock churn bounded to one Stage-A update. |
| 5 | L-R2-2 | §9.0 dev-mode override | Added `FF_BACKEND_ACCEPT_UNREADY=1` + `FF_ENV=development` dual-gate override with loud WARN + `ff_backend_unready_boot_total` metric; production refuses. |
| 6 | L-R2-3 | §16 completeness | Added `shutdown_prepare` cross-backend divergence paragraph (Valkey tail-session drain vs Postgres UNLISTEN * + pool drain) and `list_pending_waitpoints` pagination consistency paragraph (SSCAN best-effort-stable vs Postgres REPEATABLE READ exactly-once). |
| 7 | Housekeeping | `round-1/author-response.md` stale 31+14=45 count | Annotated with superseded notice; RFC body at 31+20=51 is authoritative. |

## Open items for owner adjudication

- **Q2 — `cancel_flow` dispatch ownership** (the only remaining open question after round-1 reduction). Author advisory lean: header-only on the trait; owner confirm required before Stage C merges handler 13. Both options fully specified in §12 + §16.

## Protocol compliance

- `feedback_rfc_team_debate_protocol`: two-round adversarial review (K + L as independent challenger personas), unanimous ACCEPT reached, all findings either conceded + applied or resolved with argue-back. No reviewer sits on RED.
- `feedback_approve_against_rfc_not_plan`: challengers cross-checked against §13 Alternatives-rejected, §1.4 Scope, and §13.6 anti-pattern. No smuggled middle paths.
- `feedback_rfc_quality`: polish pass applied all 7 non-blocking items in place; no deferred findings.

## State

PR #261 is ready for owner adjudication on:
1. Accept RFC-017 to `rfcs/accepted/` at polish SHA `9a905170657896ff64181aac5f548d6d87d341d0`.
2. Resolve Q2 (`cancel_flow` dispatch ownership).

No further team-debate rounds required.
