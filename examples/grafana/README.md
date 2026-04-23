# FlowFabric — Grafana Operator Dashboard

A curated Grafana dashboard (`flowfabric-ops.json`) for FlowFabric
operators. This is the 0.5.x alternative to the forthcoming Web UI
(`ff-board`) and targets the "80% of operator visibility use cases"
that the Web UI will later subsume.

## What it shows

Ten panels covering the metrics currently emitted by `ff-observability`
through its OTEL → Prometheus exposition pipeline (as of PR #170):

1. **Claim-from-grant latency (per lane)** — P50/P95/P99 over
   `ff_claim_from_grant_duration_seconds`.
2. **Claim-from-grant rate (per lane)** — observation rate of the same
   histogram (`_count` series) by lane.
3. **Lease renewals (by outcome)** — `ff_lease_renewal_total` split by
   `outcome=ok|err`.
4. **Worker-at-capacity rejections** — `ff_worker_at_capacity_total`.
5. **Admission control — quota blocks & budget breaches** —
   `ff_quota_hit_total` (reason=rate|concurrency) and
   `ff_budget_hit_total` (dimension=...).
6. **Scanner cycle duration P95** —
   `ff_scanner_cycle_duration_seconds` by `scanner` label.
7. **Scanner cycle cadence** — `ff_scanner_cycle_total` rate by
   `scanner` label.
8. **Cancel-reconciler backlog depth** — `ff_cancel_backlog_depth`
   (observable gauge).
9. **HTTP request rate (by status class)** — `ff_http_requests_total`
   aggregated to 2xx / 3xx / 4xx / 5xx via `label_replace`.
10. **HTTP P95 latency (by path)** — `ff_http_request_duration_seconds`
    P95 by matched-route `path` label.

## Importing

### Grafana UI

1. Open Grafana → **Dashboards** → **New** → **Import**.
2. Upload `flowfabric-ops.json` (or paste its contents).
3. On the import screen, pick the Prometheus datasource that scrapes
   your ff-server's `/metrics` endpoint when prompted for the
   `datasource` variable.
4. Click **Import**.

### HTTP API

Grafana's dashboard API accepts the raw JSON wrapped in a
`{"dashboard": ..., "overwrite": true}` envelope. Example using
`curl`, a service-account token, and `jq`:

```bash
GRAFANA_URL="https://grafana.example.com"
GRAFANA_TOKEN="glsa_..."   # service-account token with dashboard-write scope

jq -n --slurpfile dash examples/grafana/flowfabric-ops.json \
  '{dashboard: $dash[0], overwrite: true, message: "import FlowFabric ops dashboard"}' \
  | curl -sS -X POST "$GRAFANA_URL/api/dashboards/db" \
      -H "Authorization: Bearer $GRAFANA_TOKEN" \
      -H "Content-Type: application/json" \
      --data-binary @-
```

The dashboard is marked `editable: true`, so operators can customise
panels, thresholds, and layout after import without forking this file.

## Datasource configuration

The dashboard uses a single templated datasource variable named
`datasource` (type `prometheus`). It defaults to the name `prometheus`,
which matches the name most Helm charts and `grafana.ini` examples use
for the first Prometheus datasource.

If your datasource has a different name — e.g. `Prometheus` (capital P),
`prom-ha`, `mimir` — pick it from the datasource dropdown at the top of
the dashboard after import; the selection is saved per-user.

Your Prometheus must be configured to scrape ff-server's `/metrics`
endpoint. A minimal scrape config:

```yaml
scrape_configs:
  - job_name: flowfabric
    metrics_path: /metrics
    static_configs:
      - targets: ["ff-server.default.svc.cluster.local:8080"]
```

The `/metrics` route is only mounted when ff-server is built with the
`observability` feature and a `Metrics` registry is passed to
`api::router_with_metrics(..)` (see `crates/ff-server/src/main.rs` and
`crates/ff-server/src/api.rs`). Without that, `/metrics` returns 404
and every panel on this dashboard stays empty.

## Metric reference

All names below are the **Prometheus-exposition** names (with the
`_total` / `_seconds` suffixes that `opentelemetry_prometheus` appends
automatically). They match `crates/ff-observability/src/real.rs`
one-for-one.

| Metric | Type | Labels | Emitted from |
|---|---|---|---|
| `ff_http_requests_total` | counter | `method`, `path`, `status` | ff-server HTTP middleware (`ff-server/src/metrics.rs`) |
| `ff_http_request_duration_seconds` | histogram | `method`, `path`, `status` | ff-server HTTP middleware |
| `ff_scanner_cycle_total` | counter | `scanner` | `ff-engine/src/scanner/mod.rs` — every cycle, regardless of work done |
| `ff_scanner_cycle_duration_seconds` | histogram | `scanner` | same |
| `ff_cancel_backlog_depth` | observable gauge | — | cancel-reconciler scanner, sampled per cycle |
| `ff_claim_from_grant_duration_seconds` | histogram | `lane` | `ff-scheduler/src/claim.rs` |
| `ff_lease_renewal_total` | counter | `outcome` (ok\|err) | `ff-backend-valkey` `renew_impl` (post PR #170) |
| `ff_worker_at_capacity_total` | counter | — | `ff-scheduler` claim path |
| `ff_budget_hit_total` | counter | `dimension` | `ff-scheduler` claim path, on `HardBreach` |
| `ff_quota_hit_total` | counter | `reason` (rate\|concurrency) | `ff-scheduler` claim path |

### Label-value reference

* `scanner` — static name string provided by each scanner registration
  (e.g. `cancel_reconciler`, `lease_reclaim`, etc., depending on which
  scanners your deployment enables). The dashboard auto-discovers them
  via `sum by (scanner)` — no config needed.
* `lane` — lane label from ff-scheduler's claim-from-grant path (per
  RFC-012 lane partitioning).
* `outcome` — `ok` on successful lease renewal, `err` otherwise.
* `dimension` — budget dimension string from the flow's `budgets`
  config (e.g. `tokens`, `usd`, or user-defined).
* `reason` — `rate` for rate-quota denials, `concurrency` for
  concurrency-quota denials.
* `path` — matched Axum route template, not raw URL (keeps cardinality
  bounded per the RFC cardinality envelope of ~1 000 label-sets).

## Known scope gaps (0.5.x)

This dashboard is scoped strictly to what `ff-observability` emits
today. A few operator-relevant signals the Web UI will eventually cover
are **not** available here because no metric exists for them yet:

* **Completion outcome split** (success / failed / cancelled / expired
  rates). Completions are tracked in spans but not exported as a
  counter.
* **End-to-end claim-to-complete latency.**
  `ff_claim_from_grant_duration_seconds` covers only the claim step.
* **Queue depth per partition.** Not currently exported.
* **Active / suspended execution counts.** Not currently exported.
* **Error rate by `BackendErrorKind`.** Errors surface in logs and
  spans; no counter is emitted.
* **Worker instance count.** Not currently exported (use your service
  discovery or `up{job="flowfabric"}` instead).

Rather than invent metric names for these gaps, we defer them to
ff-observability work. When those counters land, this dashboard will
grow panels in a follow-up PR.
