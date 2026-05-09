# FlowFabric examples

Sixteen end-to-end examples exercising the public API surface. Each
subdirectory has its own `Cargo.toml` and `README.md`; run them from
the subdirectory.

Three tiers:

- **Zero external services.** Uses the SQLite dev backend
  (`FF_DEV_MODE=1`). Fastest path to a running example.
- **Valkey + `ff-server`.** Production-shaped path. Start Valkey and
  `ff-server` per the root [`README`](../README.md) quick start.
- **LLM-powered.** Requires `OPENROUTER_API_KEY`. Hits external
  inference providers.

| Example | Tier | What it demonstrates |
| --- | --- | --- |
| [`ff-dev`](ff-dev/) | zero-svc | v0.12 RFC-023 dev harness: in-process SQLite, `create_flow`, `claim`, `complete`, Wave-9 admin, RFC-019 `subscribe_completion`. |
| [`external-callback`](external-callback/) | zero-svc (SQLite) | v0.13 SC-09 waitpoint consumer flow — HMAC-signed token roundtrip, `create_waitpoint` (#435), `FlowFabricAdminClient::connect_with` (#432). Includes a tamper-rejection branch. [Migration](../docs/CONSUMER_MIGRATION_0.13.md). |
| [`incident-remediation`](incident-remediation/) | zero-svc (SQLite) | v0.12 RFC-024 SC-10 lease-reclaim flow — two-responder pager-death handoff with `ReclaimGrant` + `claim_from_reclaim_grant`; second run shows `ReclaimCapExceeded`. |
| [`v010-read-side-ergonomics`](v010-read-side-ergonomics/) | Valkey | v0.10 read-side API: flat `Capabilities::supports.<flag>` (#277), typed `LeaseHistoryEvent`, tag-restricted subscriptions via `ScannerFilter::with_instance_tag`. |
| [`retry-and-cancel`](retry-and-cancel/) | Valkey | Retry-exhaustion terminal failure + `cancel_flow` cascade. Minimal control-plane demo. |
| [`v013-cairn-454-budget-ledger`](v013-cairn-454-budget-ledger/) | Valkey | v0.13 cairn #454 per-execution budget ledger (migration 0020, Lua v31). `EngineBackend::record_spend` + `release_budget`, idempotent replay. |
| [`v014-rfc025-worker-registry`](v014-rfc025-worker-registry/) | Valkey | v0.14 RFC-025 worker registry round-trip — `register_worker` → idempotent refresh → `heartbeat_worker` → `list_workers` → `mark_worker_dead`. Atomic `ff_register_worker` FCALL (Lua v34). [Migration](../docs/CONSUMER_MIGRATION_0.14_worker_registry.md). |
| [`v016-worker-runtime`](v016-worker-runtime/) | Valkey | Issue #331 handler-DI runtime. `WorkerRuntime::new(worker).data(http).on("enrich", handler).run()` replaces the hand-rolled idle-backoff / bounded-spawn / panic-catch / shared-state loop. Feature-gated on `ff-sdk/runtime`. |
| [`v011-wave9-postgres`](v011-wave9-postgres/) | Postgres | v0.11 RFC-020 Wave 9 Postgres demo — multi-tenant operator dashboard exercising all six Wave-9 method groups (`change_priority`, `cancel_execution` + `ack_cancel_member`, `replay_execution`, `list_pending_waitpoints`, `cancel_flow_header`, `read_execution_info`). `FF_PG_TEST_URL`. |
| [`v015-ff511-scheduler-agnostic`](v015-ff511-scheduler-agnostic/) | Postgres | v0.15 FF #511 headline demo — Postgres-only deployment, `Scheduler::new_with_backend` without a `ferriskey::Client`, `release_admission` with idempotent replay. [Migration](../docs/CONSUMER_MIGRATION_0.15_scheduler_agnostic.md). |
| [`llm-race`](llm-race/) | LLM | v0.6 primitives — race N free OpenRouter providers, cancel losers with `AnyOf{CancelRemaining}`, stream the winner via `DurableSummary`. Best starting point for learning v0.6. |
| [`coding-agent`](coding-agent/) | LLM | LLM-powered code-patch worker with streaming output and human-in-the-loop suspend/signal review. |
| [`media-pipeline`](media-pipeline/) | LLM + whisper.cpp | Three-stage audio pipeline (transcribe → summarize → embed) — capability routing, stream tail with terminal markers, HMAC-signed waitpoint signals. |
| [`deploy-approval`](deploy-approval/) | Valkey | CI/CD pipeline demo — parallel test fan-out, two-distinct-source approval gate (`Count{n:2, DistinctSources}`), JSON Merge Patch build-log frames, `cancel_flow` on verify failure. |
| [`token-budget`](token-budget/) | Valkey | v0.9 UC-37 + UC-39 batch-inference runner — shared per-flow token budget, real-time `report_usage`, `cancel_pending` on hard-limit breach. |
| [`grafana`](grafana/) | operator asset | Ten-panel Grafana dashboard (`flowfabric-ops.json`) — claim-grant latency, lease renewals, admission-control hits, scanner cycle timing, HTTP status rates. Load into Grafana pointed at a Prometheus scrape of `ff-server`. |

## Running without Docker

The `ff-dev`, `external-callback`, and `incident-remediation` examples
run against the SQLite dev backend (`FF_DEV_MODE=1 --backend sqlite`).
No Valkey, no Postgres, no `ff-server` required — use these first if
you just want to see FlowFabric move.

See [`docs/dev-harness.md`](../docs/dev-harness.md) for the SQLite dev
loop and [`docs/DEPLOYMENT.md`](../docs/DEPLOYMENT.md) for the Valkey
path.

## Running every example

`scripts/run-all-examples.sh` is the mechanical release-gate entry
point — it build-checks every subdir and live-runs the ones that don't
need external credentials. See [`CLAUDE.md §5`](../CLAUDE.md) for the
release-gate contract.
