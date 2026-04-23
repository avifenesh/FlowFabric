# Open issues obsolescence sweep (2026-04-23)

Worker YY, manager-directed sweep. Mirror of the #43 discovery pattern — PR
merged with parenthetical `(#N)` but no `Closes #N`, so issue stayed open.

Scope: 9 open issues in avifenesh/FlowFabric as of 2026-04-23. One report,
investigation only, no code changes, no comments posted.

## Method

```
gh issue list --repo avifenesh/FlowFabric --state open --limit 50 \
  --json number,title,labels,createdAt,updatedAt
# Per issue N:
git log --all --oneline --grep="#$N"
gh pr list --repo avifenesh/FlowFabric --state merged --search "#$N" \
  --json number,title,mergedAt,mergeCommit --limit 5
# Plus targeted code-presence greps in crates/ff-sdk and crates/ff-core
# to verify each issue's referenced surface still exists / was rewritten.
```

## Inventory (all 9 open issues)

| # | Title | Classification |
| --- | --- | --- |
| 117 | RFC-012 Stage 1b deferrals (create_pending_waitpoint / append_frame / suspend / report_usage trait-shape) | LIVE |
| 99  | Release gate: enumerate & resolve cairn outstanding requests before 0.3.0 | LIVE (tracker) |
| 93  | BackendConfig: decouple WorkerConfig from Valkey connection primitives | LIVE |
| 92  | StreamCursor enum replacing XRANGE/XREAD wire markers | OBSOLETE |
| 89  | Extract ExecutionOperations trait covering 18 ClaimedTask FCALL methods | STALE (owner call) |
| 88  | Seal ferriskey::Error leak on SdkError and ServerError | LIVE |
| 87  | Seal ferriskey::Client leak on FlowFabricWorker + stream free-fns | LIVE |
| 51  | Questions about apalis comparison | META (external) |
| 11  | RFC-009: cap-routing convergence — V2 options | LIVE |

## Obsolete — ready to close

| # | Title | Landed-by PR | Merge SHA | Merged | Rationale |
| --- | --- | --- | --- | --- | --- |
| 92 | StreamCursor enum replacing XRANGE/XREAD wire markers | PR #125 | `70e5d80e8acf99431bea9b6b3df7ed8862d9ed55` | 2026-04-22 | Exactly the ask: opaque `StreamCursor { Start, End, At }` added to `ff-core::contracts`; SDK `read_stream` / `tail_stream` and REST `ReadStreamParams` / `TailStreamParams` flipped to `StreamCursor`; e2e fixtures migrated. `ff-sdk/src/task.rs:1868` does `pub use ff_core::contracts::StreamCursor`. PR title: `feat(ff-core,ff-sdk,ff-server): opaque StreamCursor on wire types (#92)`. The `(#92)` is parenthetical — no `Closes #92` trailer, so GitHub did not auto-close. Same pattern as #43. |

## Live — keep open

- **#117** — Tracks the 4 deferred Stage 1b methods that need trait-shape
  amendments (create_pending_waitpoint / append_frame / suspend / report_usage).
  PR #119 shipped 8 of 12 methods, PR #135 amended RFC-012 Round-7 to document
  the deferrals. The actual trait-shape fixes + final method migrations are
  NOT shipped. Keep open.
- **#99** — Release-gate tracker for 0.3.0; owner decision 2026-04-22 is that
  0.3.0 does not cut until cairn asks are drained. Active gate, not a code
  change.
- **#93** — `BackendConfig` enum NOT present. `crates/ff-sdk/src/config.rs`
  still exports `WorkerConfig { host, port, tls, cluster, ... }` and
  `worker.rs::connect(config: WorkerConfig)` still takes the flat shape. No
  PR has landed this refactor. Keep open — decoupling Phase 2 work.
- **#88** — `crates/ff-sdk/src/lib.rs:111` still has
  `Valkey(#[from] ferriskey::Error)` on `SdkError`, and `ValkeyContext`
  still carries `source: ferriskey::Error`. Typed `EngineError` from PR #81
  (58.6) is a related but distinct channel; the `ferriskey::Error` leak on
  `SdkError` itself is not sealed. Keep open.
- **#87** — `crates/ff-sdk/src/worker.rs:616` still exposes
  `pub fn client(&self) -> &Client`, and `task.rs` still has free-fn
  `read_stream` / `tail_stream` taking `&Client`. Not sealed. Keep open.
- **#11** — RFC-009 V2 cap-routing convergence. Explicit acceptance
  criteria (partial ≥ 0.99, scarce ≥ 0.95) not met; v1 best-effort shipped;
  V2 Option A / B not implemented. Keep open.

## Stale — owner call

- **#89** — "Extract ExecutionOperations trait covering 18 ClaimedTask FCALL
  methods." RFC-012 landed the trait under the name `EngineBackend`
  (`crates/ff-core/src/engine_backend.rs:76`), not `ExecutionOperations`.
  Through PRs #102 (RFC), #107 (amendment 13→15), #109 (Stage 0), #114
  (Stage 1a), #119 (Stage 1b), and #135 (Round-7 amendment), 11 of 15
  RFC-012-scoped methods are trait-shipped; the remaining 4 are deferred to
  #117 for trait-shape alignment. The ask is **substantially** satisfied but
  (a) renamed trait and (b) 4 residuals live on #117.
  Owner call: close #89 as superseded-by-RFC-012, leaving #117 as the
  residual tracker — OR keep #89 open until #117 lands. Not a #43-pattern
  auto-close. Default: LIVE, flagged for owner.

## Meta — no action

- **#51** — External user (non-team) asking questions about apalis
  comparison and offering a PR. Discussion issue, not an engineering action
  item. No merge expected to close it. Leave to owner's discretion for
  response / close-with-comment.

## Recommended immediate closes

1. **#92 — StreamCursor enum replacing XRANGE/XREAD wire markers**

   Suggested closing comment:

   ```
   Closing as obsolete. PR #125 merged 70e5d80 on 2026-04-22 landed the
   opaque `StreamCursor { Start, End, At(Bytes) }` wire type in
   `ff-core::contracts`, flipped SDK `read_stream` / `tail_stream` and
   REST `ReadStreamParams` / `TailStreamParams` off the raw XRANGE
   sentinel strings, and migrated the e2e fixtures. Squash title carried
   `(#92)` parenthetically without a `Closes #92` trailer, so GitHub did
   not auto-close — same #43-class bookkeeping miss.
   ```

## Summary

- Open issues: 9
- OBSOLETE (close-ready, high confidence): **1** — #92
- LIVE: 6 (#117, #99, #93, #88, #87, #11)
- STALE (owner call): 1 — #89
- META: 1 — #51

Only one high-confidence #43-pattern close surfaced this pass. The RFC-012
cluster (#87, #88, #89, #93) initially looked susceptible to the same
pattern because PR #102 squash mentions `(#89)` and PR #138 sweeps through
many numbers, but code-presence greps confirm #87 / #88 / #93 are NOT yet
shipped. Their labels (`decoupling-phase-2`) correctly place them as
pending EngineBackend trait prep. #89 is the only judgment call.
