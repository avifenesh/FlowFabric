# RFC-draft: Observability batteries

Investigator: Worker WWWW. Drafted 2026-04-22. Investigation + design
only — no code, no PR. Cites apalis sources (`apalis-dev/apalis`),
`rfcs/drafts/apalis-deep-architecture-dive.md` (Worker PPPP), and the
in-tree `/metrics` endpoint landed as PR-94
(`crates/ff-server/src/metrics.rs`).

Scope: decide which of apalis's four observability batteries
(`apalis-prometheus`, `apalis-opentelemetry`, `apalis-sentry`,
`apalis-board`) FlowFabric should ship, in what shape, and in which
release train.

## Motivation

Worker PPPP's apalis-deep-architecture-dive §"Out-of-box observability
batteries" (lines 232-235) flagged three gaps relative to apalis:
"No Prometheus feature flag that drops a ready exporter, no Sentry
layer, no web UI. FF has OTEL but consumers still wire their own
exporter + dashboard." Classed as "medium-impact, zero-engine-change
if we add features" — feature-list, not architecture.

Operator reality on FF today:

- **Engine metrics**: `ff-observability` (feature-gated) owns an OTEL
  `SdkMeterProvider` wired through `opentelemetry-prometheus 0.31` into
  a `prometheus::Registry`, rendered as text by `Metrics::render()`
  (`crates/ff-observability/src/real.rs:105-184`).
- **Engine `/metrics`**: PR-94 already mounts `GET /metrics` on
  `ff-server` (unauthenticated, text exposition
  `version=0.0.4`) plus an axum middleware recording
  `ff_http_requests_total` + `ff_http_request_duration_seconds` with
  `MatchedPath` cardinality caps
  (`crates/ff-server/src/metrics.rs:29-48`,
  `crates/ff-server/src/api.rs:479-488`).
- **Consumer-side metrics**: nothing. Consumers writing
  `claim_next().await?` loops in their own services wire their own
  exporter, their own Sentry, their own dashboard.
- **Tracing-to-Sentry bridge**: not wired.
- **Web UI**: none.

So PPPP's "no Prometheus exporter bundled" claim is accurate for
**consumer-side job metrics** and a **distribution** gap for the
engine (`/metrics` exists but no consumer-facing feature flag makes
Prometheus scraping turnkey). Sentry + web-UI gaps are unambiguous.

The owner-lock 2026-04-22 ("metrics=OTEL via ferriskey") sets the
ceiling: we OTEL-first, Prometheus-via-bridge. That lock does not
exclude shipping Prometheus/Sentry/board as optional batteries; it
excludes making them the primary instrumentation path.

## Scope: what we are proposing

Three batteries, independent, each opt-in behind a Cargo feature.

### 1. Prometheus endpoint (engine-side — mostly done)

**Status**: `/metrics` already exists on `ff-server` behind the
`observability` feature (PR-94). The remaining question is consumer-
side.

**Proposal**:

- Keep `/metrics` on `ff-server` as the canonical engine scrape
  target. No new crate, no duplicate endpoint.
- Do **not** add a direct `prometheus::Registry` alongside OTEL. The
  existing path (OTEL meter → `opentelemetry-prometheus` reader →
  `prometheus::Registry` → text exposition) is the OTEL-via-ferriskey
  owner lock's concrete shape. A parallel direct-Prometheus path
  would duplicate metric handles, re-declare cardinality rules, and
  invite the two registries to drift.
- Add a **consumer-side** story: document (in `ff-sdk`'s README +
  `examples/`) how a consumer running a worker loop exposes its own
  `/metrics` using the same `ff-observability` crate in `enabled`
  mode, or a thin `ff-sdk` feature `metrics-endpoint` that exposes
  an `axum::Router` returning `Metrics::render()`. No new crate.
- Feature-gate shape: existing `observability` feature on
  `ff-server`, `ff-engine`, `ff-scheduler` stays. Add
  `metrics-endpoint` feature on `ff-sdk` (pulls `axum` + re-exports
  a `metrics_router()` builder). No `ff-prometheus` crate.

**Interaction with #170's OTEL bridge**: additive. The bridge is the
source; `/metrics` is a sink. A direct Prometheus endpoint would be
duplicative; we are not adding one.

**Scope estimate**: engine `/metrics` already done. Consumer
`metrics-endpoint` feature on `ff-sdk` is ~0.5 day (axum router +
example + one integration test). Documentation is ~0.5 day.

### 2. Sentry integration

**Proposal**: ship a `sentry` feature on `ff-observability` that
installs a `sentry-tracing` layer into the `tracing_subscriber`
registry when enabled. Both engine and consumer can opt in.

**Shape**:

- New feature `ff-observability/sentry` → pulls
  `sentry = { version, default-features = false, features = ["tracing"] }`
  and `sentry-tracing`.
- New function `ff_observability::install_sentry(dsn, opts)` that
  initializes the guard and returns it; caller owns the guard's
  lifetime (drop on shutdown to flush).
- Events: forward `tracing::error!` and `tracing::warn!` by default;
  configurable via the usual sentry-tracing knobs.
- Engine-side: `ff-server` gains a `sentry` feature that
  transitively enables `ff-observability/sentry` and reads
  `FF_SENTRY_DSN` from env in `main.rs`. Off by default.
- Consumer-side: same mechanism — consumer binary enables
  `ff-observability/sentry`, calls `install_sentry`, done.

**Both** sides, because errors happen on both. No engine-only
restriction.

**Env-var config**: `FF_SENTRY_DSN`, `FF_SENTRY_ENVIRONMENT`,
`FF_SENTRY_SAMPLE_RATE`. Mirrors `sentry` crate's conventions.

**Scope estimate**: ~4 hours engine-side, ~2 hours consumer example.

### 3. Web UI (admin board)

**Proposal**: defer to 0.6.x. Not in 0.5 scope.

**Rationale**:

- apalis-board is a non-trivial artifact: a SPA + an API server that
  reads backend state. FF would need equivalents for execution list,
  lane throughput, lease-renewal rate, suspended-execution detail,
  budget state. Given RFC-012's engine-backend-trait abstraction,
  the board also has to be backend-agnostic — it cannot read Valkey
  directly.
- Minimum-viable shape: new crate `ff-board` exposing an axum
  handler + embedded HTML (no separate SPA build). Consumes public
  `ff-sdk` read APIs (list-executions, get-execution,
  list-suspended, etc.).
- Scope estimate: **>1 week** for a usable MVP (read-only list +
  detail views). Multi-week for write-operations (cancel, reclaim).
  The read-API surface on `ff-sdk` is not yet fully shaped — several
  endpoints (list-suspended with cursor, lane throughput histogram)
  would need to land first.
- Risk: an HTML admin UI becomes a supported surface. Bug reports,
  i18n pressure, accessibility. Cost is not just the initial spike.

**Recommendation**: park behind an RFC-draft for 0.6+, surface the
read-API requirements into a separate prerequisite list. Do not
commit a timeline in 0.5.

## Non-goals

- **Direct `prometheus` client in ff-observability.** OTEL-via-
  ferriskey is the owner lock; we render Prometheus text through the
  existing bridge. Not adding a parallel registry.
- **An independent `ff-prometheus` crate.** Engine `/metrics` is on
  `ff-server`; consumer `/metrics` is a thin feature on `ff-sdk`. No
  third crate.
- **Sentry as a required dependency.** Feature-gated, off by default,
  on both engine and consumer.
- **A web UI in 0.5.** Defer to 0.6+ RFC.
- **Auth on `/metrics`.** PR-94 set the convention: network-layer
  ACL, not app-layer auth. Unchanged.
- **Job-level dashboards modelled on apalis-board's workflow graph.**
  FF flows have different topology (RFC-007); we would not mimic
  apalis-board's DAG renderer, even in 0.6.

## Interaction with other RFCs

- **Worker UUUU's tower-layer RFC** (client-side composable layers):
  Sentry + Prometheus are orthogonal axes. A tower layer emits
  tracing events; `sentry-tracing` catches them; the Prometheus
  endpoint renders what the tower layer recorded. No API overlap.
  **Coordination point**: if UUUU's RFC introduces an
  `ff-sdk::observability` namespace for client-side layers, the
  `metrics-endpoint` feature proposed here should live inside it
  (same module, same feature flag discipline). Cross-link at
  implementation time.
- **#170 OTEL bridge**: this RFC is additive — Sentry is a new sink
  alongside the existing OTEL → Prometheus bridge; consumer
  `metrics-endpoint` re-uses #170's render path.
- **RFC-012 engine-backend-trait**: web UI (when we build it) must
  read via the trait, not Valkey directly. Noted as a prerequisite
  for the deferred 0.6 work.

## Alternatives rejected

1. **Use-OTEL-only; tell consumers to deploy an OTEL collector.**
   Technically sufficient — `opentelemetry-prometheus` +
   `opentelemetry-otlp` cover both scrape and push. Rejected because
   the marginal cost of a `sentry` feature is low and the
   consumer-experience delta is large: "enable a feature + set an
   env var" vs "stand up a collector, configure exporters, wire
   alerting." We're a batteries-included engine, and Sentry is a
   table-stakes battery.
2. **Bundle an opinionated Grafana dashboard JSON instead of a web
   UI.** Actively considered for 0.6. Much cheaper than a web UI and
   serves 80% of the same need. Should be a separate RFC.
3. **Separate `ff-prometheus` crate with its own registry.**
   Rejected: duplicates OTEL registration, drifts from the owner
   lock, doubles the cardinality-governance surface.
4. **Sentry engine-only, not consumer.** Rejected: consumer code is
   where flow-logic errors actually surface; engine-only Sentry
   misses the primary signal.

## Staging recommendation

| Battery | Release | Scope | Rationale |
|---|---|---|---|
| Engine `/metrics` | done (PR-94) | 0 days | already shipped |
| Consumer `metrics-endpoint` feature on `ff-sdk` | 0.5.0 | ~1 day | tiny, re-uses #170's renderer |
| Sentry feature on `ff-observability` + `ff-server` | 0.5.0 | ~0.5 day | table-stakes, feature-gated |
| Grafana dashboard JSON (bundled example) | 0.5.x | ~1 day | defer if 0.5.0 is full |
| Web UI (`ff-board`) | 0.6+ | multi-week | needs SDK read-API prerequisites; separate RFC |

0.5.0 lands two batteries (consumer-metrics + Sentry) for ~1.5 days of
work total. 0.5.x adds dashboard JSON if the release train has room.
0.6+ opens the `ff-board` RFC.

## Open questions for owner

1. **Sentry feature naming**: `ff-observability/sentry` (flat) or
   `ff-observability/sinks-sentry` (anticipating future sinks —
   Datadog, Honeycomb direct)? The flat name is simpler today;
   the prefixed name scales. Owner preference?
2. **Consumer `metrics-endpoint` crate location**: on `ff-sdk` (this
   RFC's proposal) or on a new `ff-observability-http` crate?
   Pushing onto `ff-sdk` pulls `axum` into consumer dependency
   graphs who opt in; a separate crate is cleaner but adds a
   workspace member.
3. **Web-UI commitment**: is 0.6+ deferred-for-now acceptable, or
   does the owner want a concrete milestone commitment now? If the
   latter, we need the SDK read-API prerequisite list as a parallel
   investigation.
4. **Grafana dashboard JSON**: in-tree (`examples/grafana/`) or
   out-of-tree (separate `flowfabric-dashboards` repo)? In-tree
   couples dashboard evolution to engine releases — usually what
   operators want.
5. **Env-var prefix for Sentry**: `FF_SENTRY_*` (matches other FF
   env vars) or `SENTRY_*` (matches sentry crate defaults so
   consumers can reuse existing ops config)? Small, but picks a
   support boundary.

## Scope estimate

- Consumer `metrics-endpoint` on `ff-sdk`: **0.5 day** (axum router,
  example, integration test).
- Sentry feature + env wiring: **0.5 day** (feature flag, installer,
  main.rs integration, doc).
- Documentation pass covering both + the already-existing `/metrics`:
  **0.5 day** (new section in `ff-observability` README +
  `docs/operator-guide.md` entry).
- **Total 0.5.0 scope**: ~1.5 days of one-worker time.
- Grafana dashboard JSON (optional 0.5.x): ~1 day.
- Web UI (0.6+): separate RFC, separate estimate, **>1 week MVP**.

## References

- `rfcs/drafts/apalis-deep-architecture-dive.md` (Worker PPPP),
  §"Observability" lines 181-189 + §"Out-of-box observability
  batteries" lines 232-235.
- `crates/ff-observability/src/real.rs` lines 69-184 (existing OTEL
  → Prometheus bridge).
- `crates/ff-server/src/metrics.rs` lines 1-70 (PR-94 `/metrics`
  endpoint + HTTP middleware).
- `crates/ff-server/src/api.rs` lines 479-488 (unauthenticated
  `/metrics` mount).
- Owner lock 2026-04-22: "metrics=OTEL via ferriskey" (cited in
  manager notes `project_ff_release_decisions.md`).
- apalis sources: `apalis-dev/apalis/apalis-prometheus`,
  `apalis/src/layers/sentry`, `apalis/src/layers/opentelemetry`,
  `apalis-dev/apalis-board` (web UI).
