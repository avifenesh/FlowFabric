# ff-board SDK prerequisite audit (2026-04-23)

Investigator: Worker AAAAA. Investigation-only — no code, no PR.
Inputs: `rfcs/drafts/RFC-draft-observability-batteries.md` (Worker WWWW,
§Scope 3 lines 120-146 + §Non-goals lines 161-163 + §Open-questions-3),
owner commit 2026-04-22 moving Web UI into **0.5.0** scope, current
`ff-sdk` public surface, current `EngineBackend` trait.

Purpose: list the SDK read-APIs that a minimal `ff-board` v1 would
bind to, classify which already exist vs. which must land before
`ff-board` can start, and file prerequisite issues so the Web UI spike
is not blocked on discovery.

Hard scope guard (from the accepted observability RFC, §Non-goals): no
apalis-board DAG-workflow renderer mimicry; FF flow topology differs
(RFC-007). We scope to **read-only operator panels**, not DAG editing.

## 1. MVP Web UI scope

`ff-board` v1 — read-only, backend-agnostic (via `EngineBackend`
trait where possible, falling back to `ff-server` HTTP where the
trait doesn't yet cover), embedded HTML + axum handler:

| # | Panel | Primary user question |
|---|---|---|
| P1 | **Execution list** (paged, filter by state: eligible/delayed/active/suspended/terminal) | "What's in flight right now?" |
| P2 | **Execution detail** (state, attempts, leases, resume signals, parent flow) | "Why is this one stuck?" |
| P3 | **Suspended-execution list** (cursor-paginated, waitpoint reason) | "What's waiting on signals?" |
| P4 | **Lane list + throughput** (per-lane: depth, claim rate, lease miss rate, recent N-minute window) | "Is a lane backed up?" |
| P5 | **Lease renewal health** (global + per-lane renewal success/miss rate) | "Are workers healthy?" |
| P6 | **Stream tail** (live frames for an attempt, for debugging a running execution) | "What is this execution doing?" |
| P7 | **Flow summary** (flow snapshot + member executions + edges) | "What's the shape of this flow?" |
| P8 | **Error/retry rate per lane** (windowed) | "Which lanes are failing?" |

Explicit out-of-MVP: DAG topology renderer (non-goal per RFC);
write-operations — cancel/reclaim/signal (deferred to v2; operator
can use `ff-sdk` admin CLI).

## 2. Per-panel SDK needs

Legend: `[E]` = exists and is usable today; `[S]` = exists on
`ff-server` HTTP but no `ff-sdk` wrapper; `[B]` = exists on backend
state but neither `ff-sdk` nor `EngineBackend` trait exposes it;
`[M]` = not available, must be built.

| Panel | Today's surface | Gap |
|---|---|---|
| P1 execution list | `ff-server` `GET /v1/executions?partition&lane&state&offset&limit` → `ListExecutionsResult`. `Server::list_executions` is Valkey-ZRANGE on lane indexes. No `ff-sdk` wrapper. Not on `EngineBackend` trait. `[S]` | Add `ff-sdk` wrapper `list_executions(partition, lane, state, cursor)`. Ideally promote to `EngineBackend::list_executions` (cross-backend question — Postgres shape differs). |
| P2 execution detail | `EngineBackend::describe_execution` + `observe_signals` + `resolve_execution_flow_id` all exist; `ff-sdk::snapshot::describe_execution` wraps. `[E]` | None for the snapshot. Attempts/leases already in `ExecutionSnapshot`. |
| P3 suspended list | Server `list_executions(state="suspended")` returns eids only — no waitpoint-reason, no cursor (offset/limit-based). RFC flagged "list-suspended cursor" explicitly. `[B]` | Cursor-paginated variant returning `ExecutionId + waitpoint_reason + suspended_at`. SDK wrapper. Backend primitive: ZSCAN on suspended ZSET + HMGET reason. |
| P4 lane list + throughput | Lanes themselves: there is **no `list_lanes` API** anywhere. Throughput: `ff_claim_from_grant_duration` histogram + `ff_lease_renewal_total` exist on `/metrics` but are **not** exposed as a structured per-lane read API — only as Prometheus text. `[M]` for lanes + throughput. | New SDK fn `list_lanes(partition) -> Vec<LaneSummary>` (depth, oldest-eligible age, active-count). New SDK fn `lane_throughput(lane, window) -> LaneThroughput` (claims/sec, p50/p99 claim latency, fail rate). |
| P5 lease renewal | `ff_lease_renewal_total{outcome}` metric exists; no structured read API. `[M]` | New SDK fn `lease_health(window)` OR document that ff-board consumes the Prometheus scrape directly. Recommend a thin SDK helper that parses the local `/metrics` text → struct, not a new backend trait method (metric origin is the right source of truth). |
| P6 stream tail | `EngineBackend::read_stream` + `tail_stream` exist (streaming feature). `ff-server` HTTP endpoints exist (`GET /v1/executions/{id}/attempts/{idx}/stream[/tail]`). **No `ff-sdk` wrapper.** `[S]` | SDK wrapper `read_attempt_stream` + `tail_attempt_stream` (already-shaped backend, missing convenience). |
| P7 flow summary | `EngineBackend::describe_flow` + `list_edges` + `describe_edge` exist. `ff-sdk::snapshot` wraps. **No `list_flows` anywhere** — can't enumerate flows without knowing IDs. `[E]` for detail, `[M]` for enumeration. | New SDK fn `list_flows(partition, cursor) -> Vec<FlowSummary>`. Needs backend primitive (currently flows aren't indexed in a scan-friendly way — cross-backend question). |
| P8 error/retry rate | Per-lane retry/error rate is **not emitted as a metric today** (only claim-duration + lease-renewal). `[M]` | Either (a) add `ff_attempt_outcome_total{lane,outcome}` counter to `ff-observability`, and have ff-board read via /metrics parse, or (b) new SDK fn `lane_error_rate(lane, window)`. Preference: (a) — reuses existing metrics pipeline. |

## 3. Classification

| Bucket | Count | Items |
|---|---|---|
| **Free wrappers** (data exists, SDK convenience needed) | 3 | (F1) `ff-sdk::list_executions` wrapper over `/v1/executions`; (F2) `ff-sdk::read_attempt_stream` + `tail_attempt_stream` wrappers; (F3) `ff-sdk::list_pending_waitpoints` wrapper over `/v1/executions/{id}/pending-waitpoints`. |
| **New trait methods** (EngineBackend extension, `#[non_exhaustive]` additive, backend impl needed) | 3 | (T1) `list_executions(partition, lane, state, cursor) -> ListExecutionsPage`; (T2) `list_suspended(partition, cursor) -> SuspendedPage` with reason; (T3) `list_lanes(partition) -> Vec<LaneSummary>` (depth + counts). |
| **Cross-backend questions** (data shape differs; needs decision before trait signature) | 3 | (X1) `list_flows` — Valkey has no flow index, Postgres would; should it be in the trait at all, or a backend-specific extension? (X2) `lane_throughput` — metrics are observability-plane data, not engine-state; decide: trait method, SDK helper over `/metrics` text, or OTEL-only (ff-board queries Prometheus directly)? (X3) `lease_health` — same question as X2. |
| **New metrics** (no new API, just emit + document) | 1 | (N1) `ff_attempt_outcome_total{lane,outcome}` counter added to `ff-observability` so P8 can read it from `/metrics`. |

Net: **10 prerequisite items** for an MVP ff-board — 3 free, 3
new-trait, 3 cross-backend (requiring owner decision before
implementation), 1 metric-only.

Recommendation: resolve the 3 cross-backend questions first (X1/X2/X3
are architectural and the outcome changes how much work T1–T3 are).
For X2/X3 specifically, we recommend **"ff-board reads /metrics
directly"** — the observability RFC's owner lock is "metrics=OTEL via
ferriskey"; routing lane throughput back through the engine-backend
trait duplicates the cardinality/naming governance already centralized
in `ff-observability`. ff-board is local-to-engine (same axum server
or co-deployed) and can scrape `/metrics` over loopback.

That collapses X2 + X3 into "no new API" → effective prereq count
drops to 7 (3 free + 3 new-trait + 1 metric).

## 4. Filed issues

Grouping: **single umbrella tracking issue with a checklist** plus
**one standalone issue per new-trait-method** (T1/T2/T3) and
**X1 as a design decision** since that blocks trait signatures.
F1/F2/F3/N1/X2/X3 stay as checklist items under the umbrella —
small + don't need RFC-level discussion (X2/X3 have a recommended
resolution pending owner confirm).

| # | Issue | Kind |
|---|---|---|
| [#181](https://github.com/avifenesh/FlowFabric/issues/181) | ff-board Web UI SDK prerequisites (0.5.0 tracking) | Umbrella |
| [#182](https://github.com/avifenesh/FlowFabric/issues/182) | EngineBackend: `list_executions` trait method | T1 |
| [#183](https://github.com/avifenesh/FlowFabric/issues/183) | EngineBackend: `list_suspended` cursor-paginated trait method | T2 |
| [#184](https://github.com/avifenesh/FlowFabric/issues/184) | EngineBackend: `list_lanes` trait method | T3 |
| [#185](https://github.com/avifenesh/FlowFabric/issues/185) | Design question: `list_flows` cross-backend shape | X1 |

## 5. Effort estimate

Prerequisites **before ff-board itself starts**:

| Item | Kind | Estimate |
|---|---|---|
| Cross-backend decisions (X1/X2/X3) — owner lock | discussion | 0.5 day (async) |
| F1 list_executions wrapper | free | 0.5 day (incl. test) |
| F2 stream read/tail wrappers | free | 0.5 day |
| F3 list_pending_waitpoints wrapper | free | 0.25 day |
| T1 EngineBackend::list_executions + Valkey impl | trait | 1.0 day |
| T2 EngineBackend::list_suspended + Valkey impl (reason field + cursor) | trait | 1.5 days |
| T3 EngineBackend::list_lanes + Valkey impl | trait | 1.0 day |
| N1 `ff_attempt_outcome_total` counter + wiring | metric | 0.5 day |
| Doc pass on new SDK surface | doc | 0.5 day |
| **Prereq total** | | **~6 days** (1 worker) |

`ff-board` itself (per observability RFC §3): **>1 week MVP** for
read-only list + detail; multi-week for write-ops. We recommend
**read-only MVP only** in 0.5.0 (~1.5 weeks), deferring write-ops to
0.6.x.

**0.5.0 total ff-board budget**: ~6 days prereqs + ~7 days MVP ≈
**2.5–3 weeks** of one-worker time, assuming the cross-backend
decisions resolve quickly. Longer if X1 (list_flows) requires its
own sub-RFC.

## 6. Hard scope lines drawn

- No DAG renderer (RFC Non-goal, line 162-163).
- No write-operations in v1 (cancel/reclaim/signal) — operator uses
  existing SDK/CLI.
- No auth on ff-board (same convention as `/metrics`, PR-94, §Non-
  goals line 159-160 of the observability RFC — network-layer ACL).
- No separate SPA build — embedded HTML in the `ff-board` crate per
  the observability RFC §3 lines 132-135.
- ff-board consumes `/metrics` over loopback for throughput/health
  (X2/X3 resolution recommended above); does NOT re-implement
  metric collection.

## 7. Open decisions for owner (block implementation of T1–T3)

1. **X1 `list_flows`**: is a flow-enumeration primitive in scope for
   0.5.0, or do we scope ff-board v1 to "flow detail only, given an
   ID from an execution"? The latter is cheaper and defers the
   cross-backend question.
2. **X2/X3 throughput & lease-health data path**: confirm
   recommended approach — ff-board scrapes `/metrics` directly, no
   new trait method. If rejected, we must design a structured
   read API for observability-plane data.
3. **list_executions pagination shape**: server today is
   `offset/limit`; trait should be `cursor`-based to work for
   Postgres. OK to make the server endpoint cursor-based at the
   same time (backwards-incompatible on the unreleased HTTP
   surface)?
4. **N1 attempt-outcome metric cardinality**: `{lane,outcome}` —
   outcome ∈ {ok, error, timeout, cancelled, retry}. 5 outcomes ×
   ≤16 lanes = 80 series. Within existing cardinality budget. OK?
