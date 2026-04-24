# RFC drafts — exploration record

This directory holds RFC drafts that were explored but **not merged as RFCs**. They are preserved as an exploration-and-reasoning record, not as authoritative design documents. Do not cite them as policy.

Each cluster of files represents one exploration. Conventions:

- `RFC-<NNN>-amendment-<topic>.md` — the draft under revision.
- `RFC-<NNN>-amendment-<topic>.<letter>-challenge.md` — a named challenger's adversarial review.

## Index

### RFC-017 ff-server backend abstraction (accepted 2026-04-23)

Tracks the `ff-server` backend-abstraction RFC (A + B divergent drafts → consolidated master → K/L challenger rounds → owner adjudication on Q2).

- `RFC-017-challenges/RFC-017-ff-server-backend-abstraction-A.md` — Author A's divergent draft (PR #259, superseded by consolidated master).
- `RFC-017-challenges/RFC-017-ff-server-backend-abstraction-B.md` — Author B's divergent draft (PR #260, superseded by consolidated master).
- `RFC-017-challenges/ACCEPTED.md` — debate-record closure summary (both challengers ACCEPT round 2).
- `RFC-017-challenges/round-1/author-response.md` — per-finding concede/argue-back record for K's 12 findings + L's 5 deltas.

**Outcome:** consolidated master promoted to `rfcs/RFC-017-ff-server-backend-abstraction.md` as the authoritative final. Owner adjudicated the sole remaining Q2 on 2026-04-23: `cancel_flow` is header-only on the trait; callers poll `describe_flow` for sync-cancel semantics. RFC-017 merged via PR #261; promotion via this PR. Implementation kicks off at Stage A (§9).

### 2026-04-23 session exploration records (0.4.0 readiness + RFC-009 V2 + Stage-1c followups)

Investigation-only artifacts from the 2026-04-23 session covering 0.4.0 release-readiness, RFC-009 V2 cap-routing dispositioning, Stage-1c planning refinement, observability coverage, and an open-issue obsolescence sweep. Not normative RFCs — preserved as chain-of-reasoning and evidence. Mirrors the 2026-04-22 archive pattern (PR #138).

- `0.3.4-recheck-smoke.md` — Worker HHH, compile-only recheck of published v0.3.4 crates mirroring Worker X's 0.3.2 smoke. **Verdict:** green on default + `--no-default-features`; headline APIs type-check. **Status:** landed-elsewhere (informed 0.4.0 audit).
- `0.4.0-release-readiness-audit.md` — Worker III, snapshot of commits since v0.3.4 and in-flight pieces expected in the 0.4.0 bundle. **Verdict:** readiness inventory (6 merged commits since v0.3.4 enumerated). **Status:** still-referenced (input to 0.4.0 tag planning).
- `apalis-comparison.md` — Worker VV, response to issue #51 comparing apalis vs FlowFabric and assessing maintainer's shared-connection claim. **Verdict:** comparison + substantive-claim disposition. **Status:** explored-not-pursued as an RFC (feeds issue #51 reply).
- `issue-43-scheduler-perf.md` — Worker (manager-directed), investigation of #43 bounded-scan + observability ask; notes PR #86 landed the partition-affine rotation. **Verdict:** issue stale — PR merged without `Closes #43`. **Status:** landed-elsewhere (candidate for issue close).
- `observability-coverage-audit.md` — Worker OOO, post-Stage-1c-T3 audit of tracing/metrics/backend_context coverage across the 17 `EngineBackend` methods. **Verdict:** coverage matrix with gap-list; task brief's `Corruption` / `list_edges` references do not match code. **Status:** still-referenced (input to Stage-1c followups).
- `open-issues-obsolescence-sweep.md` — Worker YY, manager-directed sweep of 9 open issues following the #43 pattern (merged PRs with parenthetical `(#N)` but no `Closes #N`). **Verdict:** per-issue classification (live vs obsolete). **Status:** still-referenced (triage input).
- `rfc-009-status-summary.md` — Worker XX, consolidation of RFC-009's status vs issue #11 (cap-routing V2 options), RFC-011 / RFC-012 touches, and the scheduler-perf work. **Verdict:** issue #11 is LIVE; v1 shipped best-effort; V2 Options A/B not implemented. **Status:** still-referenced (pairs with Worker EEE's analysis).
- `rfc-009-v2-real-world-analysis.md` — Worker EEE, assessment of whether the cap-routing thrash residual is a real production concern or bench-contrived. **Verdict:** (see file §Recommendation). **Status:** still-referenced (input to issue #11 disposition).
- `stage-1c-plan-draft.md` — Worker SS-v4, revision of Stage-1c plan adopting the owner's trait feature-gate decision + v3→v4 changelog. **Verdict:** effort 34-50h; tranche scope with `#[cfg(feature = ...)]` scaffolding in tranche 1. **Status:** still-referenced (Stage-1c execution plan).

### 2026-04-22 session records (v0.3.x release saga + investigations)

Retrospective, audit, and investigation artifacts from the 2026-04-22 v0.3.x release saga and concurrent Stage 1c / cairn-unblock work. Not normative RFCs — preserved as chain-of-reasoning, retrospective, and evidence. Cross-references PRs #119, #120–#124, #126–#128, #130–#136.

- `release-saga-2026-04-22-post-mortem.md` — Worker JJ's post-mortem of the v0.3.x release saga (what failed, what held, what changed).
- `0.3.2-smoke-report.md` — Worker X's smoke finding that caught the ScannerFilter + CompletionBackend blockers before tag.
- `stage-1c-scope-audit.md` — Worker EE's Stage 1c scope inventory (what's in, what's out, what's deferred).
- `backend-timeouts-retry-audit.md` — Worker FF's `BackendTimeouts` / `BackendRetry` field audit (which fields are wired, which are placeholders).
- `scenario-4-regression-investigation.md` — Worker DD's Scenario 4 bisect finding: methodology artifact, not a regression.
- `bridge-event-audit.md` — Worker V's bridge-event audit surfacing GAP-1 + GAP-2 that shipped in v0.3.1.
- `87-88-scope-carveout.md` — Worker BB's #87/#88 scope audit and carve-out.
- `RFC-012-amendment-117-deferrals.{K,L,M}-challenge.md` — challenger records for the #117 amendment (amendment itself landed in PR #135; challengers kept as exploration record).

### RFC-012 #117 deferrals amendment (explored 2026-04-22, promoted as Round-7)

Tracks issue [#117](https://github.com/avifenesh/FlowFabric/issues/117) (Stage 1b deferrals — three `ClaimedTask` methods that couldn't land as thin forwarders).

- `RFC-012-amendment-117-deferrals.{K,L,M}-challenge.md` — three adversarial review rounds (Worker K round-1, Worker L round-2, Worker M round-3).

**Outcome:** draft v4 promoted to `rfcs/RFC-012-engine-backend-trait.md` §R7 as the Round-7 amendment. The draft body was folded into the RFC; the three challenger reports are preserved here as the exploration-and-reasoning record. The amendment ships `create_waitpoint` (new method), `append_frame` (return widen), `report_usage` (return replace); `suspend` is deferred wholesale to Stage 1d.

### RFC-012 namespace amendment (explored 2026-04-22, not pursued)

Tracks issue [#122](https://github.com/avifenesh/FlowFabric/issues/122) (cairn's multi-tenant isolation ask).

- `RFC-012-amendment-namespace.md` — draft v5 of a backend-level namespace-prefix amendment.
- `RFC-012-amendment-namespace.{K,L,M,P}-challenge.md` — four adversarial review rounds.

**Outcome:** the amendment grew to ~500 lines for one cairn request. After round-4 (Worker P) surfaced that `Namespace` already existed in `ff-core::types`, a peer-team dialog with cairn (in the #122 thread) right-sized the fix to a `ScannerFilter { namespace, instance_tag }` construction-time filter — no key-shape change, no RFC amendment. Shipped as PR [#127](https://github.com/avifenesh/FlowFabric/pull/127).

The draft is kept for three reasons:

1. Chain-of-reasoning for why the full prefix amendment was wrong-sized.
2. Record of the four-round challenger discipline pattern (K → L → M → P), reusable for future high-impact amendments.
3. Reference for if operator-level isolation (distinct from cairn's tenant/instance filter) becomes needed later.
