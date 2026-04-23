# Observability coverage audit (post-Stage-1c-T3, 2026-04-23)

Scope: the 17 public methods on `ff_core::engine_backend::EngineBackend`
(crates/ff-core/src/engine_backend.rs:77-231) and their Valkey impls in
`crates/ff-backend-valkey/src/lib.rs` (impl block at lib.rs:1450-1609).
Audit axes: (1) `tracing::instrument`/`span!`/`in_scope`, (2)
`ff_observability::Metrics` handle calls on the trait boundary,
(3) `backend_context(...)` error-context helper coverage,
(4) `Validation { detail }` specificity at decoder / wire-parse sites.

Note on task brief: the brief references an `EngineError::Validation`
`Corruption` variant and a `list_edges` 17th trait method. Neither
exists in the current tree. `ValidationKind` is defined at
engine_error.rs:117-165 (22 variants, no `Corruption`); the trait has
exactly 17 methods today and `list_edges` lives in `ff-sdk` snapshot
code (crates/ff-sdk/src/snapshot.rs:723-802), not on the trait. The
matrix below reflects the code as it stands.

Note on `backend_context`: the helper lives in ff-sdk
(crates/ff-sdk/src/lib.rs:246) and ff-server (re-export) — NOT in
ff-backend-valkey. The backend crate lifts ferriskey errors through
`backend_error_from_ferriskey` (crates/ff-backend-valkey/src/backend_error.rs:74)
and raw `EngineError::from(ScriptError::Parse { ... })` constructions.
So the column below reads "n/a (no helper in crate)" for all 17 rows
rather than a boolean — the gap is crate-level, not per-method.

## Method matrix

| Method | Tracing span | Metric | Error-context helper | Notes |
|---|---|---|---|---|
| claim | none | none | n/a (Unavailable stub, lib.rs:1452-1459) | Not wired; scheduler does claim via direct ff-sdk path. |
| renew | SDK-layer span `renew_lease` at ff-sdk/src/task.rs:967-971 on `renew_once` wrapper; impl at lib.rs:1461-1464 has none | `inc_lease_renewal` handle exists (ff-observability/src/real.rs:235) but is only fired from tests (ff-server/tests/metrics.rs:56); no production call site | raw `transport_fk` + `ScriptError::Parse` at lib.rs:486-492 | Parse message carries `fcall` + raw field string; no execution_id on this specific site. |
| progress | none | none | raw (progress_impl lib.rs:498-534) | — |
| append_frame | none | none | raw Parse at lib.rs:682-688 (includes fcall + execution_id + raw value) | — |
| complete | none | none | complete_impl lib.rs:1081-1139; no Parse sites (all `.map_err(EngineError::from)`) | — |
| fail | none | none | fail_impl lib.rs:1240+; no Parse site in body | — |
| cancel | none | none | cancel_impl lib.rs:1141-1238 (Parse site at lib.rs:1386 has fcall + execution_id + sub-status context) | — |
| suspend | none | none | n/a (Unavailable stub, lib.rs:1516-1523) | — |
| create_waitpoint | none | none | create_waitpoint_impl lib.rs:796-866; Parse at lib.rs:842 (includes fcall + execution_id) | — |
| observe_signals | none | none | observe_signals_impl lib.rs:536-721; decoder failures bubble as transport | — |
| claim_from_reclaim | none | none | n/a (Unavailable stub, lib.rs:1550-1557) | — |
| delay | none | none | delay_impl lib.rs:986-1028 | — |
| wait_children | none | none | wait_children_impl lib.rs:1030-1079 | — |
| describe_execution | none | none | n/a (Unavailable stub, lib.rs:1573-1580) | Real impl lives in ff-sdk/src/snapshot.rs and DOES use `backend_context` extensively (snapshot.rs:86-92). |
| describe_flow | none | none | n/a (Unavailable stub, lib.rs:1582-1589) | Same; ff-sdk snapshot path uses helper. |
| cancel_flow | none | none | cancel_flow_fcall lib.rs:316-406; Parse at lib.rs:382-385 includes fcall + flow_id | — |
| report_usage | none | none | report_usage_impl lib.rs:868-984; dedicated `parse_u64_field` helper lib.rs:965-980 with field_name + index + raw-value detail | Best-in-crate parse-detail hygiene; use as template. |

Summary line: 1/17 trait methods have a tracing span (renew, via
SDK wrapper only), 0/17 emit metrics on the backend boundary,
0/17 go through `backend_context` (helper not reachable from the
crate), 5/17 are `Unavailable` stubs so their error surface is
trivially clean.

## Gaps by category

### Tracing
- 16/17 methods have zero span instrumentation at either the trait
  impl (lib.rs:1450-1609) or the `*_impl` functions (renew_impl etc.).
- The lone span is `renew_lease` on a one-line wrapper
  (ff-sdk/src/task.rs:972-978), added for a bench harness, not as a
  crate-wide policy.
- Four `tracing::warn!` sites in the backend (lib.rs:137, lib.rs:590,
  lib.rs:1162, lib.rs:1438) + four in completion.rs (224, 241, 251,
  263, 330, 341, 352, 364) are local recovery logs, not spans — they
  don't carry the method name / handle id so a grep-by-op in a log
  aggregator has to pattern-match strings.

### Metrics
- Crate-level: `ff-backend-valkey` does not depend on
  `ff-observability`. Zero metric emission from the trait impl.
  (Verified: no matches for `ff_observability` in
  crates/ff-backend-valkey/src/*.rs or Cargo.toml.)
- `ff_observability::Metrics::inc_lease_renewal` (real.rs:235) is
  defined but only called from `ff-server/tests/metrics.rs:56` — there
  is no production emission of the lease-renewal counter today.
- Budget/quota counters fire from `ff-scheduler::claim` (claim.rs:907,
  1006, 1013) on the SDK path, not from the trait's `report_usage`
  impl. A consumer using `Arc<dyn EngineBackend>` directly (as
  RFC-012's ownership story invites) gets zero budget telemetry.
- `record_claim_from_grant` fires from `ff-scheduler` (claim.rs:437),
  parallel story to the above.

### Error context (`backend_context`)
- Helper is ff-sdk-local (`ff-sdk/src/lib.rs:246`, re-exported by
  ff-server). `ff-backend-valkey` lifts via
  `backend_error_from_ferriskey` (backend_error.rs:74) which populates
  `BackendError::Valkey { kind, message }` but does NOT attach a
  per-call-site op-name string the way `backend_context(e, "claim:
  HGETALL ...")` does in ff-sdk.
- Effect: a `Transport` error surfaced from e.g. `renew_impl` carries
  ferriskey kind + its bare message, not "renew: FCALL
  ff_renew_lease" context. Callers triaging logs have to infer the op
  from surrounding spans — and see above, those spans don't exist.
- This is the biggest observability regression introduced by the
  Stage 1c seal: ff-sdk's snapshot paths still annotate, but every
  hot-path op that moved behind the trait lost the annotation.

### Validation detail specificity
- Handle-codec decoder (handle_codec.rs:93-132, 153-158, 170-215)
  does this well: `invalid()` helper emits
  "invalid Valkey backend handle: <field> <what> at offset <pos>,
  have <len>" messages. Offsets + field names both present.
- Wire-parse sites are good but inconsistent:
  - Best: `parse_u64_field` (lib.rs:965-980) — fcall + execution_id +
    sub-status + field name + index + raw value.
  - Good: `ff_report_usage_and_check` unknown-sub-status (lib.rs:954-958)
    — fcall + execution_id + raw.
  - Thin: `ff_renew_lease` expires_at parse (lib.rs:486-492) — fcall
    + raw, no execution_id (available via `f.execution_id`).
  - Thin: `load_partition_config` path (lib.rs:382-385) — fcall +
    flow/exec label, no field name.
- No field shape / expected-type detail anywhere (e.g. "expected
  non-negative i64" — the message is just "invalid expires_at: foo").

## Recommendations

Ordered by impact. No code edits in this audit.

**Blocking for 0.4.0** — none. The gaps are ergonomic, not
correctness. Consumers can attach their own tracing subscriber and
still diagnose; they just have less structured detail.

**High-value (cheap, big obs win)**
1. Add `#[tracing::instrument(name = "ff.<method>", skip_all,
   fields(execution_id, handle_kind, backend = "valkey"))]` to each
   of the 12 non-stub `*_impl` functions in
   ff-backend-valkey/src/lib.rs. One line per function; no behavior
   change; lets consumers filter by `ff.renew`, `ff.complete`, etc.
2. Add `ff-observability` dep to `ff-backend-valkey` (behind the
   `observability` feature it already has conditional support for)
   and thread `Arc<Metrics>` into `ValkeyBackend` so the 12 non-stub
   methods can record op-timing + outcome counters. Parity with the
   scheduler's `record_scanner_cycle` pattern.
3. Wire the existing `inc_lease_renewal` counter at `renew_impl`'s
   success/failure branches. The handle is already defined and
   tested; only call sites are missing.
4. Promote `backend_context` from ff-sdk to ff-core (module
   `engine_error::context`). Have each `*_impl` in
   ff-backend-valkey wrap its `ferriskey` calls with
   `backend_context(e, "renew: FCALL ff_renew_lease")` etc. Restores
   the context annotation the Stage-1c seal dropped.

**Low-priority (polish)**
5. Bring `ff_renew_lease` parse detail up to `parse_u64_field`
   standard — add execution_id + expected-type to its message
   (lib.rs:486-492).
6. Add `expected_shape: &'static str` to the `ScriptError::Parse`
   struct (ff-script side) so parse messages are structurally
   diffable, not bag-of-strings.
7. Consider a `ValidationKind::Corruption` variant (the brief's
   mental model) for handle-decoder shape errors — today they route
   through `InvalidInput`, which conflates caller-error (bad arg)
   with backend-error (corrupt opaque). Low-priority because the
   detail string already disambiguates in practice.

## Effort estimate

| Recommendation | Effort | Touch surface |
|---|---|---|
| 1. `#[instrument]` on 12 `*_impl` fns | ~30 min + test-run | 12 attrs in lib.rs |
| 2. Wire `ff-observability` into `ValkeyBackend` | ~1 day | Cargo.toml, `BackendConfig`, constructor, 12 call sites |
| 3. Fire `inc_lease_renewal` | ~15 min | renew_impl success/error arms |
| 4. Promote `backend_context` + annotate 12 impls | ~3 hrs | ff-core new module, ff-sdk re-export, 12-20 wrap sites |
| 5. Thicker `ff_renew_lease` parse | ~10 min | lib.rs:486-492 |
| 6. `expected_shape` on ScriptError::Parse | ~2 hrs + migration | ff-script struct + N call sites |
| 7. `ValidationKind::Corruption` | ~1 hr | engine_error.rs enum + decoder routing + tests |

Total high-value bucket ≈ 1.5 days. Fits in 0.4.x without a release
slip.
