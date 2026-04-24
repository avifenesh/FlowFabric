# v0.7 migration master — debate-record marker

**Status:** ACCEPTED (unanimous, both challengers, after round 2 + polish pass).
**Target:** `rfcs/drafts/v0.7-migration-master.md`
**Owning branch:** `docs/v0.7-migration-master` — PR #229
**Debate protocol:** high-impact RFC, all-worker debate until unanimous ACCEPT (per team-debate protocol memo).

## Outcome

- Challenger K (correctness / atomicity / reconciler / dual-backend invariants) — **ACCEPT** at round 2, two remaining correctness risks + four clarification questions, all folded into polish pass.
- Challenger L (cairn operations / migration / SDK deltas) — **ACCEPT** at round 2, two residual follow-ups (Q12 additive-boot-check branch, Q15 forward-only-cutover surfacing), both folded into polish pass.

Owner sign-off (merge of PR #229) is the next gate. This record marks the debate itself closed.

## Round 1 — findings (resolved in commit `5cc456a`)

Challenge artifacts: `rfcs/drafts/v0.7-master-challenges/round-1/{K,L}.md` on the respective debate branches.

**Challenger K (round 1) — 12 findings:**

1. K-1 — isolation-level self-contradiction between executive summary and §L0.
2. K-2 — cascade recursion transaction boundaries (per-hop vs one-fat-tx) unstated.
3. K-3 — shared LISTEN channel as an SPOF; no reconnect contract.
4. K-4 — NOTIFY-in-tx atomicity unclear vs outbox durability.
5. K-5 — per-row NOTIFY trigger storm risk if misread as the design.
6. K-6 — Q6 MAXLEN retention under tx contention.
7. K-7 — RFC-015 MAXLEN characterisation (approximate-N is the deliberate choice, not a bug).
8. K-8 — Q9 scanner drop proposal went too far; `budget_reconciler` + `quota_reconciler` must stay.
9. K-9 — Stage A/B boundary deliverable set.
10. K-10 — dual-backend failure modes unspecified.
11. K-11 — fence-triple preservation across backends.
12. K-12 — HMAC `kid` rollout + cutover union-seed.

**Challenger L (round 1) — 11 findings:**

- Op-gap 1 — schema migration ordering (Q12).
- Op-gap 2 — dual-backend running mode (→ §8).
- Op-gap 3 — metrics cardinality / labels-compat matrix.
- Op-gap 4 — backup / PITR shape.
- Op-gap 5 — reconciler-as-primary observability counters.
- SDK-delta 1 — per-partition HMAC no-op on non-partitioned backends.
- SDK-delta 2 — `ts_ms` monotonicity under PITR.
- SDK-delta 3 — tail reconnect storm.
- SDK-delta 4 — `backend_version()` pin policy (Q13).
- SDK-delta 5 — new `BackendErrorKind` retry classes through ff-sdk.
- Exec-summary 8-week framing calibration.

All 23 round-1 findings resolved in revision commit `5cc456a` (round-1 revision, reviewed 2026-04-23).

## Round 2 — residuals (folded in polish pass)

Challenge artifacts: `round-2/K.md` on `rfc/v0.7-master-debate-K-r2`, `round-2/L.md` on `rfc/v0.7-master-debate-L-r2`.

**Challenger K (round 2) — 5 residuals:**

1. Q11 retry-exhaust should fall back to the appropriate reconciler rather than surface `SerializationFailure` to the SDK (cascade is idempotent; reconciler backstop already exists).
2. Q12 rolling-upgrade policy silent on stored-proc signatures — needed an additive-only / `_v2`-suffix rule.
3. §8.1 overclaimed `EngineError::Validation("cross-backend")`; actual behavior is `NotFound` at the ff-server local lookup miss (federation detection is structurally impossible without a bridge layer).
4. §8.2 / Q15 drain had no bound for long-running HITL suspensions; needed explicit per-flow co-migrate-or-wait policy.
5. Stage D missed flagging that `dependency_reconciler` on Postgres owns a transitive-descendant sweep — a real semantic delta from the Valkey single-hop model, not a transplant.

**Challenger L (round 2) — 2 residuals:**

- L-r2-1 — Q12 self-contradiction between the additive-matrix (old binaries stay read-write) and the earlier "read-only if ledger ahead" clause; resolved by annotating migrations `backward_compatible=true|false` and branching the boot check on the annotation.
- L-r2-2 — Q15 forward-only rollback was buried in Q15 + §8.2; needed an executive-summary callout so a cairn operator reading the summary does not mis-assume per-tenant reversibility.

All 7 residuals folded into polish commit `0e5582a` — no new design decisions introduced, no residuals escalated as needing new thinking.

## Commits

| Stage | SHA | Message |
|---|---|---|
| Round-1 revision | `5cc456a` | `rfc(v0.7-master): round-1 revisions addressing K+L challenges` |
| Round-2 polish (final) | `0e5582a` | `rfc(v0.7-master): round-2 polish — K+L non-blocking clarifications` |

## Next gate

Owner review + merge of PR #229. Debate record closed; no further worker rounds planned.
