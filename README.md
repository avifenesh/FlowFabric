# FlowFabric

[![crates.io](https://img.shields.io/crates/v/flowfabric.svg)](https://crates.io/crates/flowfabric)
[![CI](https://github.com/avifenesh/FlowFabric/actions/workflows/matrix.yml/badge.svg)](https://github.com/avifenesh/FlowFabric/actions/workflows/matrix.yml)
[![security & quality](https://github.com/avifenesh/FlowFabric/actions/workflows/security-and-quality.yml/badge.svg)](https://github.com/avifenesh/FlowFabric/actions/workflows/security-and-quality.yml)
[![license: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Valkey 7.2+](https://img.shields.io/badge/Valkey-7.2%2B-informational)](https://valkey.io)
[![rustc edition 2024](https://img.shields.io/badge/rustc-edition%202024-dea584)](https://doc.rust-lang.org/edition-guide/rust-2024/)

**Valkey-native durable execution engine for Rust.** Long-running,
interruptible, budget-aware workflows with lease-based claim, HMAC-signed
human-in-the-loop waitpoints, capability routing, and first-class
backends for Valkey, Postgres, and SQLite.

FlowFabric is for teams who already run Redis/Valkey (or Postgres) and
want durable execution with Rust-native performance, without standing
up Kafka or adopting a workflow DSL. Workflows are ordinary `async fn`
Rust — no compiler plugins, no DAG builder, no proprietary runtime.

## Why FlowFabric

- **Use this when** you need durable background jobs with retries,
  lease-safe crash recovery, and deterministic cancellation cascades.
- **Use this when** your workflow needs to suspend for human approval
  (code review, deploy gate, two-source approval) and resume on an
  HMAC-signed signal from outside your trust boundary.
- **Use this when** you need per-flow or per-tenant token/cost budgets
  that terminate in-flight work when breached — LLM fan-out, batch
  inference, CI matrices.
- **Use this when** you want Redis/Valkey queue semantics *and*
  first-class Postgres persistence as deployment options, picked per
  environment by a feature flag.
- **Use this when** you want to embed the worker SDK directly in your
  Rust service — `WorkerRuntime::new(worker).data(http).on("task", handler).run()`
  drops the hand-rolled claim-loop boilerplate.

## Installation

```toml
# Cargo.toml — workers and clients
[dependencies]
ff-sdk = "0.15"
```

Add the umbrella crate if you want backend selection behind a
feature flag:

```toml
flowfabric = "0.15"                                    # valkey (default)
# or, to swap backends:
flowfabric = { version = "0.15", default-features = false, features = ["postgres"] }
```

See [`crates.io/crates/flowfabric`](https://crates.io/crates/flowfabric).

## Quick start

Three commands get you a running worker loop.

**1. Start Valkey.** FlowFabric requires Valkey ≥ 7.2 (RFC-011 §13) and
refuses Redis at boot. CI exercises 7.2 and 8.

```bash
docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
```

**2. Start `ff-server`.** The HMAC secret is mandatory — the server
refuses to start without it (RFC-004 §Waitpoint Security).

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) cargo run -p ff-server
```

**3. Run an example.**

```bash
# Zero-Docker path — in-process SQLite dev backend, no Valkey needed
FF_DEV_MODE=1 cargo run --bin ff-dev

# Or the WorkerRuntime handler-DI demo against the running ff-server
FF_HOST=127.0.0.1 FF_PORT=6379 cargo run --bin v016-worker-runtime
```

Full example index: [`examples/`](examples/). Fifteen runnable
examples plus one operator-dashboard asset, covering LLM fan-out,
approval gates, waitpoint roundtrip, retry exhaustion, budget ledger,
and worker-registry lifecycle.

## Core concepts

- **Execution.** A single unit of work with state, lease, attempt
  history, and optional streaming output.
- **Flow.** A DAG of executions with dependency edges, fan-out,
  cancellation cascade, and skip propagation.
- **Lease.** A worker's ownership claim on an execution. Auto-renewed
  via heartbeat; reclaimed on expiry so crashes don't wedge work.
- **Waitpoint.** A named suspension point in a flow. A worker calls
  `suspend`; an external signer POSTs an HMAC-signed token; the flow
  resumes.
- **Budget / quota.** Per-dimension usage counters with hard/soft
  limits, sliding-window rate limiting, and per-flow admission
  control.
- **Capability.** A string a worker advertises (e.g. `"gpu"`,
  `"trusted"`); the scheduler subset-matches against per-execution
  requirements before issuing a claim grant.

## Features

- **Durable execution** over Valkey Functions (FCALL) or Postgres
  SQL — backend chosen per deployment, same SDK.
- **Lease-safe claim/renew/fail** — a crashed worker releases its
  leases on TTL expiry; retry semantics are at-least-once with
  idempotent terminal ops.
- **Suspend / signal / resume** with HMAC-signed waitpoint tokens —
  safe to hand to untrusted external services for callbacks.
- **Flow coordination** — DAG execution with `cancel_flow` cascade,
  dependency edges, fan-out, skip propagation, `AnyOf` / `Count{n, DistinctSources}` joins.
- **Streaming output** — append-only frame streams scoped per attempt;
  JSON Merge Patch `DurableSummary` for incremental UI updates.
- **Priority + capability scheduling** — score-based eligible sets,
  priority clamping, capability subset matching.
- **Budget / quota** — per-flow token budgets with real-time
  `report_usage`, hard-limit `cancel_pending`, sliding-window rate
  limits.
- **Client-local middleware** — compose `TracingLayer`,
  `RateLimitLayer`, `MetricsLayer`, `CircuitBreakerLayer` around any
  `EngineBackend`.
- **29-endpoint REST API** on axum with JSON errors, CORS, Bearer
  auth, `/healthz`, optional `/metrics`.
- **Observability** — structured `tracing` logs, OTEL + Prometheus
  via `ff-observability`, optional Sentry integration, bundled
  Grafana dashboard.
- **Cluster-safe** — every key carries a hash tag
  (`{fp:N}`, `{b:M}`, `{q:K}`) so related state always co-locates;
  cluster-aware enumeration via ferriskey's hash-tag-aware
  `cluster_scan`.

## Backends

| Backend | Status at v0.15 | Use case |
| --- | --- | --- |
| **Valkey** (`FF_BACKEND=valkey`, default) | production, standalone + cluster | High-throughput durable execution. Every trait method backed by an atomic Lua FCALL. |
| **Postgres** (`FF_BACKEND=postgres`) | production, RFC-017 Stage E4 | Enterprise persistence, long-retention analytics, shops that prefer SQL ops over Redis ops. Parity matrix in [`docs/POSTGRES_PARITY_MATRIX.md`](docs/POSTGRES_PARITY_MATRIX.md). |
| **SQLite** (`FF_BACKEND=sqlite`) | **dev-only**, RFC-023 | Zero-Docker local dev loop and `cargo test` path. Gated behind `FF_DEV_MODE=1`; refuses to start without it. Not a deployment target. |

Backend selection is per-deployment via `FF_BACKEND`; the SDK and HTTP
surface are identical. See [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)
for the full env-var reference.

## Architecture

```
                         ┌─────────────┐
                         │  ff-server  │  HTTP API + 17 scanners
                         └──────┬──────┘
                                │
                 ┌──────────────┼──────────────┐
          ┌──────┴──────┐ ┌────┴────┐ ┌───────┴───────┐
          │  ff-engine  │ │ ff-sdk  │ │ ff-scheduler  │
          └──────┬──────┘ └────┬────┘ └───────┬───────┘
                 └─────── ff-script ──────────┘  (Valkey FCALL)
                                │
                         ┌──────┴──────┐
                         │   ff-core   │  traits, types, keys
                         └──────┬──────┘
           ┌────────────────────┼────────────────────┐
  ┌────────┴────────┐  ┌────────┴────────┐  ┌───────┴────────┐
  │ ff-backend-     │  │ ff-backend-     │  │ ff-backend-    │
  │  valkey         │  │  postgres       │  │  sqlite        │
  │  (production)   │  │  (production)   │  │  (dev-only)    │
  └─────────────────┘  └─────────────────┘  └────────────────┘
```

## Crates

| Crate | Role |
| --- | --- |
| [`flowfabric`](crates/flowfabric) | Umbrella re-export. Feature-flagged backend selection (`valkey` default, `postgres` opt-in). |
| [`ff-sdk`](crates/ff-sdk) | Worker SDK — public API for worker authors. Includes the `EngineBackendLayer` tower surface and the `runtime` feature for `WorkerRuntime`. |
| [`ff-core`](crates/ff-core) | Core types, state enums, partition math, key builders, `EngineBackend` trait. |
| [`ff-engine`](crates/ff-engine) | Cross-partition dispatch + 17 background scanners. |
| [`ff-scheduler`](crates/ff-scheduler) | Claim-grant cycle, fairness, capability matching, admission control. |
| [`ff-script`](crates/ff-script) | Typed FCALL wrappers and Lua library loader. |
| [`ff-backend-valkey`](crates/ff-backend-valkey) | `EngineBackend` over Valkey FCALL. |
| [`ff-backend-postgres`](crates/ff-backend-postgres) | `EngineBackend` over Postgres (RFC-017 Stage E). |
| [`ff-backend-sqlite`](crates/ff-backend-sqlite) | `EngineBackend` over SQLite (RFC-023) — **dev-only**, gated on `FF_DEV_MODE=1`. |
| [`ff-server`](crates/ff-server) | HTTP API, Valkey/Postgres connection, boot sequence, `/metrics`. |
| [`ff-observability`](crates/ff-observability) | OTEL + Prometheus + optional Sentry. |
| [`ff-observability-http`](crates/ff-observability-http) | Consumer-side `/metrics` axum router for workers embedding `ff-sdk` in library mode. |
| [`ff-test`](crates/ff-test) | Integration-test harness, fixtures, assertions. |
| [`ff-readiness-tests`](crates/ff-readiness-tests) | Release-gate behavioral smokes. |
| [`ferriskey`](ferriskey) | Valkey client (in-tree fork of glide-core with hash-tag-aware `cluster_scan`). |

## When not to use FlowFabric

- **Not an analytics pipeline.** Optimized for integrating durable
  execution into application code, not for large fan-out ETL or
  data-warehousing.
- **Not a Kafka replacement.** No topic fan-out, no consumer groups,
  no log retention semantics.
- **At-least-once semantics.** Terminal ops (`complete`, `fail`,
  `cancel`) are idempotent on retry by the same caller, but user code
  must tolerate duplicate attempt execution on worker crash.
- **SQLite is not a deployment target.** It exists for `cargo test`
  and contributor onboarding. Production is Valkey or Postgres.
- **No built-in TLS or rate limiting on `ff-server`.** Deploy behind a
  reverse proxy (nginx, Envoy). See
  [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).
- **`ff-sdk::claim_next()` bypasses admission control** — gated
  behind the default-off `direct-valkey-claim` feature. Production
  consumers use `ff-scheduler`'s claim-grant cycle.
- **Pre-1.0.** Breaking changes are documented per release in
  [`CHANGELOG.md`](CHANGELOG.md) and `docs/CONSUMER_MIGRATION_*.md`.

## Production notes

- **`cancel_flow` dispatch is asynchronous by default** — durable
  against crashes via a per-flow-partition `cancel_backlog` ZSET; the
  `cancel_reconciler` scanner drains on its interval (default 15 s).
  Append `?wait=true` for synchronous completion.
- **Terminal op replay is idempotent.** A retry by the same caller
  (matching `lease_epoch` + `attempt_id`) returns the same outcome as
  the original call would have; a retry after a *different* terminal
  op committed surfaces as `ExecutionNotActive` with populated
  `terminal_outcome`.
- **KEYS-arity changes require blue-green deploys.** Lua `LIBRARY_VERSION`
  bumps pair with this constraint — see
  [`docs/DEPLOYMENT.md §7 Upgrade checklist`](docs/DEPLOYMENT.md#7-upgrade-checklist).
- **Lua auto-reload after Valkey failover.** The server reloads the
  library on first "function not found" error in a new epoch; the
  triggering request fails once and retries.
- **Auth is opt-in.** If `FF_API_TOKEN` is unset, every endpoint
  except `GET /healthz` is reachable unauthenticated.

## Documentation

- [`CHANGELOG.md`](CHANGELOG.md) — release history, one entry per
  user-visible change.
- [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) — production deployment
  guide, full env-var reference, Docker Compose starter.
- [`docs/POSTGRES_PARITY_MATRIX.md`](docs/POSTGRES_PARITY_MATRIX.md)
  — per-method backend parity table.
- [`docs/MIGRATIONS.md`](docs/MIGRATIONS.md) — Postgres schema
  migration reference.
- [`docs/CONSUMER_MIGRATION_v0.8.md`](docs/CONSUMER_MIGRATION_v0.8.md)
  … [`CONSUMER_MIGRATION_0.15_scheduler_agnostic.md`](docs/CONSUMER_MIGRATION_0.15_scheduler_agnostic.md)
  — per-release consumer migration guides.
- [`docs/RELEASING.md`](docs/RELEASING.md) — release-gate contract
  (see also [`CLAUDE.md §5`](CLAUDE.md)).
- [`docs/operator-guide-postgres.md`](docs/operator-guide-postgres.md)
  — Postgres operator runbook.
- [`docs/dev-harness.md`](docs/dev-harness.md) — local dev loop with
  the SQLite backend.
- [`rfcs/`](rfcs/) — active RFC drafts. Accepted RFCs live in the
  `avifenesh/flowfabric-archive` private repo.

## CI & quality gates

- [`matrix.yml`](.github/workflows/matrix.yml) — 6-job host × Valkey × mode
  matrix. linux x86_64 + arm64 × Valkey 7.2 + 8 × {standalone, cluster},
  plus macOS arm64.
- [`security-and-quality.yml`](.github/workflows/security-and-quality.yml)
  — `cargo audit` (deny warnings), `cargo deny`, `cargo geiger`
  ratchet, CodeQL, `cargo machete`, `cargo semver-checks` against the
  last tag.
- [`release.yml`](.github/workflows/release.yml) — tag-triggered
  verify → smoke → publish → published-artifact smoke → GitHub Release.

## Contributing

The repo does not yet ship a `CONTRIBUTING.md`. For now:

- Bugs, proposals, and questions → open an issue on
  [avifenesh/FlowFabric](https://github.com/avifenesh/FlowFabric/issues).
- Design discussions for non-trivial changes → check
  `avifenesh/flowfabric-archive` (private) for accepted RFCs before
  drafting new ones; active drafts live under [`rfcs/`](rfcs/).
- See [`CLAUDE.md`](CLAUDE.md) for the contributor-facing working
  style (surgical changes, simplicity first, release-gate
  requirements).

## License

Apache-2.0. See the `license` field in [`Cargo.toml`](Cargo.toml) and
each crate's `Cargo.toml`. The full license text is at
<https://www.apache.org/licenses/LICENSE-2.0>.
