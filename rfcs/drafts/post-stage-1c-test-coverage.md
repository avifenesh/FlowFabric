# Post-Stage-1c test coverage audit (2026-04-23)

Audit target: `EngineBackend` trait surface in
`crates/ff-core/src/engine_backend.rs` (18 methods post-Round-7 +
Stage-1c T2 `list_edges`).

Scope: unit tests in the method's home crate, integration tests in
`crates/ff-test/tests/` that exercise the method **via the trait**
(not the SDK free-fn / FCALL bypass), and error-path coverage.

## Method coverage matrix

Legend:
- UT = unit test in `ff-backend-valkey` (or another home crate).
- IT-T = integration test calling the **trait method** directly.
- IT-S = integration test that reaches the backend impl **only via
  the SDK hot-path forward** (so the trait dispatch is exercised but
  not asserted independently).
- Stub = impl currently returns `EngineError::Unavailable` (no real
  wire behaviour to cover).

| # | Method | UT | IT-T | IT-S | Error-path | Notes |
|---|--------|----|------|------|-----------|-------|
| 1 | `claim` | none | none | none | n/a | Stub (lib.rs:1619). SDK `claim_next` bypasses the trait entirely. Zero coverage. |
| 2 | `renew` | none | none | YES — `task.rs:977` renewal task exercised in every lifecycle test (e.g. `e2e_lifecycle.rs:611`) | NO direct test of `StaleLease` / `LeaseExpired` typed returns through the trait | Trait forwards a wired impl (lib.rs:1626); only happy-path via SDK. |
| 3 | `progress` | none | none | partial — SDK `update_progress` is wired to direct FCALL, **not** the trait (grep of `self.backend.progress` = zero hits in ff-sdk) | none | Trait impl wired (lib.rs:1631) but **no caller in-tree** routes through it. Effective coverage: ZERO. |
| 4 | `append_frame` | none | none | partial — `task.append_frame` still direct-FCALL; trait `append_frame` has no SDK forwarder (grep `self.backend.append_frame` = 0) | none | Trait impl wired (lib.rs:1648) but no in-tree caller routes through it. Effective coverage: ZERO. |
| 5 | `complete` | none | none | YES — SDK `task.complete` forwards via `self.backend.complete` (`task.rs:499`); exercised ubiquitously (`e2e_lifecycle.rs:4303,4803,…`) | replay path exercised (`test_complete_replay_returns_enriched_detail` at `e2e_lifecycle.rs:1441`); transport-failure path not exercised at trait level | Good happy-path, weak error-path. |
| 6 | `fail` | none | none | YES — SDK `task.fail` → `self.backend.fail` (`task.rs:554`); `test_fail_with_retry` / `test_fail_terminal` (`e2e_lifecycle.rs:1161,1251`) | retry-scheduled + terminal-classified outcomes covered; transport retry idempotency not covered through trait | Reasonable. |
| 7 | `cancel` | none | none | YES — `task.cancel` → `self.backend.cancel` (`task.rs:581`); `test_create_claim_cancel_from_active` (`e2e_lifecycle.rs:747`) | cancel-of-already-cancelled replay not asserted at trait level | Reasonable happy-path, thin error-path. |
| 8 | `suspend` | none | none | none | n/a | Stub (lib.rs:1690). SDK `task.suspend` is direct FCALL, does **not** forward through trait. Zero coverage. |
| 9 | `create_waitpoint` | none | none | partial — SDK `task.create_pending_waitpoint` is direct-FCALL per `task.rs:170` comment; grep `self.backend.create_waitpoint` = 0 | none | Trait impl wired (lib.rs:1698) but no in-tree caller routes through it. Effective coverage: ZERO. |
| 10 | `observe_signals` | none | none | YES — `task.rs:865` forwards via `self.backend.observe_signals`; `resume_signals_integration.rs` (4 tests) exercises it | `resume_signals_empty_for_non_resumed_claim` covers the non-Resumed-kind error shape | Best-covered hot-path method. |
| 11 | `claim_from_reclaim` | none | none | none | n/a | Stub (lib.rs:1719). SDK `claim_from_reclaim_grant` does direct FCALL (worker.rs:1285). Zero coverage. |
| 12 | `delay` | none | none | YES — `task.rs:454` forwards; `test_delay_execution` (`e2e_lifecycle.rs:786`) | none | Single happy-path test. |
| 13 | `wait_children` | none | none | YES — `task.rs:469` forwards; exercised in flow lifecycle tests (`e2e_lifecycle.rs:4020,5345`) | none | Happy-path only. |
| 14 | `describe_execution` | none | none | partial — SDK `worker.describe_execution` (`snapshot.rs:66`) uses direct HGETALL pipeline, **not** the trait; no in-tree caller routes through the trait impl | none | Trait impl wired (lib.rs:1737) but dead-in-test. Effective coverage: ZERO at the trait boundary. |
| 15 | `describe_flow` | none | none | partial — SDK `worker.describe_flow` (`snapshot.rs:389`) direct HGETALL; same story as #14 | none | Effective coverage: ZERO at the trait boundary. |
| 16 | `list_edges` | none | YES — `engine_backend_list_edges.rs:199` `list_edges_trait_matches_sdk_free_fn` asserts trait result equals SDK free-fn | n/a | no corruption / missing-flow error-path assertion via trait | Only method with a dedicated trait-boundary test. |
| 17 | `cancel_flow` | none | none | partial — `ff-server` uses direct FCALL (`server.rs:1612`), not the trait; `cancel_flow_backlog.rs` / `e2e_lifecycle.rs:7171` hit the FCALL, not the trait method | none | Trait impl wired (lib.rs:1757) but no in-tree caller. Effective coverage: ZERO at the trait boundary. |
| 18 | `report_usage` | none | YES — `r7_backend_report_usage.rs:104` `r7_report_usage_trait_round_trip` calls `backend.report_usage` directly | partial — the test asserts all four `ReportUsageResult` variants | no dedicated transport-fault test | Best-covered read/admin method at trait level. |

## Gaps by category

### Zero coverage (no test exercises the method, directly or indirectly)

- `claim` (#1) — stub, no callers.
- `suspend` (#8) — stub, no callers.
- `claim_from_reclaim` (#11) — stub, no callers.

Stubs-returning-`Unavailable` are defensible for today, but once the
RFC-012 Stage-1d hot-path migration lands, these become real methods
and inherit the surrounding zero-coverage posture unless tests are
added in the same PR.

### Integration-only gap: trait wired but no caller routes through it

These methods have a real wired impl in `ff-backend-valkey`, but
no SDK code-path and no test calls the trait method. A
`todo!()`/panic substituted into the trait impl would not fail any
existing test:

- `progress` (#3)
- `append_frame` (#4)
- `create_waitpoint` (#9)
- `describe_execution` (#14)
- `describe_flow` (#15)
- `cancel_flow` (#17)

Risk: Stage-1d migration could ship a broken trait impl that CI
never catches.

### Error-path gaps (happy-path covered, failure modes not)

Every method except `report_usage` (#18, covers all four result
variants) has minimal or zero error-path coverage at the trait
boundary. Specific untested failure modes:

- `renew`: `State::StaleLease`, `State::LeaseExpired` typed
  returns — tested via Lua error strings in FCALL tests, not
  surfaced through the trait's `Result<LeaseRenewal, EngineError>`
  shape.
- `complete` / `fail` / `cancel`: replay / fence-triple-mismatch
  paths exist but are asserted at SDK/FCALL layer, not against the
  trait return.
- `observe_signals`: non-`Resumed` handle kind (tested), but
  corruption / transport variants not.
- `list_edges`: missing-flow and adjacency-endpoint-drift paths are
  tested **via SDK free-fn** (`describe_edge_api.rs:385`), not via
  trait.
- `report_usage`: transport-fault variant untested.

No tests anywhere induce `EngineError::Transport` (Valkey
unavailable / ferriskey client error) through the trait for any
method — the error-surface section of the trait rustdoc is
unverified.

## Prioritization

Ranking axes:
1. Cairn exposure — will cairn-rs call this method via the trait
   under the 0.4.0 migration?
2. Change risk — was this method added or reshaped recently?
3. Stub status — zero-behaviour impls can't regress today but will
   land with hot-path behaviour in Stage-1d.

| Rank | Method | Cairn | Change-risk | Rationale |
|------|--------|-------|-------------|-----------|
| P0 | `list_edges` error-path | HIGH (Stage-1c T2, new surface cairn edge-listing hits) | HIGH (just added) | Trait test exists but corruption/missing-flow not asserted via trait. |
| P0 | `describe_execution` / `describe_flow` via trait | HIGH (cairn's primary read API) | MED | Wired impls completely untouched by tests; SDK bypasses them. |
| P0 | `report_usage` transport-fault | HIGH (budget enforcement blocks cairn admissions) | HIGH (Round-7 reshape) | All happy-path variants covered; transport fault would corrupt consumer retry logic silently. |
| P1 | `append_frame` / `progress` / `create_waitpoint` via trait | MED | LOW (wired impls stable) | Dead-in-test trait impls; Stage-1d migration will break silently if a typo lands. |
| P1 | `cancel_flow` via trait | MED (operator tools, cairn admin) | MED | Server bypasses trait today; any 0.4.x consumer who uses the trait directly hits an untested code-path. |
| P2 | `complete`/`fail`/`cancel` replay via trait | MED | LOW | SDK-layer replay tests exist; trait-return assertion is belt-and-braces. |
| P2 | `renew` typed-state returns | LOW (SDK renewal task handles) | LOW | Assertion gap; behaviour exercised. |
| P3 | `claim` / `suspend` / `claim_from_reclaim` | n/a (stubs) | n/a until Stage-1d | Defer until hot-path migration; add tests in the migration PR. |

## Recommendations

### Must-add (pre 0.4.0 ship)

1. **Trait-boundary parity test for `describe_execution` +
   `describe_flow`** — mirror the `engine_backend_list_edges.rs`
   pattern: assert `backend.describe_*` returns the same snapshot
   as the SDK free-fn for a seeded execution/flow. This closes the
   largest "wired but untested" hole and protects the cairn read
   surface.
2. **`list_edges` error-path via trait** — missing-flow returns
   `Ok(vec![])` (or similar) and adjacency-drift returns
   `Validation{Corruption}`; both are tested via SDK today
   (`describe_edge_api.rs:385,442`), port one of each to the
   trait.
3. **`report_usage` transport-fault test** — induce a ferriskey
   transport error (drop connection mid-FCALL or point at a dead
   socket) and assert `EngineError::Transport` with a
   `ScriptError`-downcastable inner. This is the one method that
   asserts its result enum exhaustively; pairing that with a
   transport assertion fully closes its error surface.

### Nice-to-add (0.4.x)

- Trait-boundary tests for `progress`, `append_frame`,
  `create_waitpoint`, `cancel_flow` — one happy-path each. Cheap
  once the pattern from Must-add #1 exists.
- `renew` typed-state test: claim, let lease expire on the wire,
  call `backend.renew` and assert `LeaseRenewal` variant (if the
  return type distinguishes) or `EngineError::State{StaleLease}`.
- Replay-idempotency assertions at the trait layer for `complete` /
  `fail` / `cancel` (one test each).

### Acceptable-as-is

- `claim`, `suspend`, `claim_from_reclaim` — stubs; tests land with
  Stage-1d hot-path migration, not before. Tracking their zero
  coverage as a pre-merge gate on the Stage-1d PR is sufficient.
- `delay`, `wait_children`, `observe_signals` — SDK-forwarded and
  exercised by lifecycle + resume tests; error-path gap is
  acknowledged but the happy-path posture is solid.

## Summary

- 18 trait methods total.
- 3 are stubs (acceptable deferral).
- 6 are wired but have **zero in-tree callers** through the trait
  (silent-breakage risk in Stage-1d).
- 2 have direct trait-boundary tests (`list_edges`, `report_usage`).
- 7 are exercised via SDK forward paths (trait dispatch runs, but
  return-value assertions are at the SDK layer).
- 0 methods have induced-transport-fault coverage through the
  trait.
- Error-path coverage is thin across the board; `report_usage` is
  the sole method asserting all of its typed result variants.
