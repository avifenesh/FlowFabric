# FlowFabric

Valkey-native execution engine for long-running, interruptible, resource-aware workflows.

## Architecture

```
                         ┌─────────────┐
                         │  ff-server   │  HTTP API + boot sequence
                         └──────┬──────┘
                                │
                 ┌──────────────┼──────────────┐
                 │              │              │
          ┌──────┴──────┐ ┌────┴────┐ ┌───────┴───────┐
          │  ff-engine   │ │ ff-sdk  │ │ ff-scheduler  │
          │  17 scanners │ │ worker  │ │ claim-grant   │
          └──────┬──────┘ │   API   │ │   cycle       │
                 │        └────┬────┘ └───────┬───────┘
                 │             │              │
                 └──────────────┼──────────────┘
                                │
                         ┌──────┴──────┐
                         │  ff-script  │  typed FCALL wrappers (Valkey)
                         └──────┬──────┘
                                │
                         ┌──────┴──────┐
                         │   ff-core   │  types, state, keys, errors,
                         │              │  EngineBackend trait
                         └──────┬──────┘
                                │
           ┌────────────────────┼────────────────────┐
           │                    │                    │
  ┌────────┴────────┐  ┌────────┴────────┐  ┌───────┴────────┐
  │ ff-backend-     │  │ ff-backend-     │  │ ff-backend-    │
  │  valkey         │  │  postgres       │  │  sqlite         │
  │  (production)   │  │  (production)   │  │  (dev-only)    │
  │                 │  │                 │  │  RFC-023       │
  └────────┬────────┘  └─────────────────┘  └────────────────┘
           │
  ┌────────┴────────┐
  │    ferriskey    │  Valkey client (Rust)
  └─────────────────┘
```

## Features

- **Lease-based ownership** -- workers hold leases on executions; auto-renewed, crash-safe
- **Suspend / signal / resume** -- human-in-the-loop and async event-driven workflows with HMAC-signed waitpoint tokens
- **Flow coordination** -- DAG execution with dependency edges, fan-out, skip propagation
- **Budget and quota** -- per-dimension usage tracking, hard/soft limits, sliding-window rate limiting
- **Streaming output** -- append-only frame streams scoped to each attempt
- **Priority scheduling** -- score-based eligible sets with priority clamping
- **Capability routing** -- workers advertise capabilities; scheduler subset-matches per-execution requirements
- **REST API** -- 27 endpoints on axum with JSON error handling, CORS, health check
- **Backend trait** -- `EngineBackend` is the stable surface for alternate backends (Valkey + Postgres both first-class at v0.8.0, RFC-017 Stage E4); `capabilities()` returns a flat `Capabilities { identity, supports }` — dot-access `caps.supports.<method>` bools (v0.10, #277); typed `subscribe_lease_history` / `subscribe_completion` / `subscribe_signal_delivery` with a required `&ScannerFilter` parameter for per-tenant tag isolation (pass `&ScannerFilter::default()` for the unfiltered stream) (v0.10, #282); cursor-paginated `list_executions` / `list_flows` / `list_lanes` / `list_suspended` for operator tooling
- **Client-local tower-style layer surface** -- opt-in `TracingLayer`, `RateLimitLayer`, `MetricsLayer`, `CircuitBreakerLayer` compose around any `EngineBackend`
- **Observability** -- OTEL via `ff-observability`, Prometheus scrape endpoint via `ff-server` (engine-side) or `ff-observability-http` crate (consumer-side), optional Sentry integration, Grafana dashboard JSON bundled
- **Cluster-safe** -- all operations use hash-tag partitioning; cluster-aware enumeration via ferriskey's hash-tag-aware `cluster_scan` (single-shard routing when the match pattern embeds a tag)

## Quick start

### 1. Start Valkey

FlowFabric is Valkey-native and is not tested or supported against Redis.
The version check expects Valkey's `INFO` shape; pointing `ff-server` at a
Redis server is unsupported and is rejected at boot by the version gate
(`parse_valkey_version` requires `server_name:valkey` in INFO). The
`redis_version:` fallback exists for pre-8.0 Valkey (which emits only
`redis_version:`, not `valkey_version:`), not to legitimize or allow Redis.

FlowFabric requires Valkey >= 7.2 (RFC-011 §13, enforced at boot). 7.2 is the
release where the Valkey Functions API + RESP3 stabilized. Valkey 7.0 and
earlier are rejected by `ff-server`.

```bash
docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
```

The quickstart image pins major 8 for reproducibility, but any `valkey/valkey`
tag whose version is ≥ 7.2 will boot. CI exercises both 7.2 and 8.

### 2. Start the FlowFabric server

`FF_WAITPOINT_HMAC_SECRET` is required at boot — the server refuses to start
without it so waitpoint HMAC authentication can never be silently disabled
(see RFC-004 §Waitpoint Security). Any even-length hex string works for
local dev; production should use 64 hex chars (32 bytes) from a secret
manager.

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) cargo run -p ff-server
```

### 3. Try an example

Seven end-to-end examples live under [`examples/`](examples/):

- **[`ff-dev`](examples/ff-dev/)** -- v0.12 headline demo for the RFC-023 SQLite dev-only backend. Try FlowFabric in 60 seconds with zero Docker: in-process `:memory:` SQLite, `create_flow` + `create_execution` + `claim` + `complete` + Wave-9 admin (`change_priority`, `cancel_execution`) + `read_execution_info` + RFC-019 `subscribe_completion` surface. Runs as `FF_DEV_MODE=1 cargo run --bin ff-dev`. No external services.
- **[`v011-wave9-postgres`](examples/v011-wave9-postgres/)** -- v0.11 headline demo for the RFC-020 Wave 9 Postgres release. Multi-tenant operator dashboard exercising all six Wave-9 method groups on Postgres: budget/quota admin, `change_priority`, `cancel_execution` + `ack_cancel_member`, `replay_execution`, `list_pending_waitpoints`, `cancel_flow_header`, and `read_execution_info`. Requires `FF_PG_TEST_URL`.
- **[`v010-read-side-ergonomics`](examples/v010-read-side-ergonomics/)** -- v0.10 headline demo for consumer read-side APIs: flat `Capabilities::supports.<flag>` discovery (#277), typed `LeaseHistoryEvent` from `subscribe_lease_history` (#282), and tag-restricted subscriptions via `ScannerFilter::with_instance_tag(..)`. Multi-tenant lease-audit console pattern. No external dependencies beyond a running `ff-server`.
- **[`llm-race`](examples/llm-race/)** -- race N free OpenRouter LLM providers against the same prompt; automatically cancel losers when one wins; stream the winner via `DurableSummary` with JSON Merge Patch. Exercises v0.6 `AnyOf{CancelRemaining}` + `Count{DistinctSources}` + typed `SuspendArgs`. Verified live 2026-04-24. Best starting point for learning v0.6 primitives. Requires `OPENROUTER_API_KEY` (free tier).
- **[`coding-agent`](examples/coding-agent/)** -- LLM-powered code-patch worker with streaming output and human-in-the-loop suspend/signal review. Requires `OPENROUTER_API_KEY`.
- **[`media-pipeline`](examples/media-pipeline/)** -- three-stage audio pipeline (transcribe → summarize → embed) exercising capability routing, stream tail with terminal markers, and HMAC-signed waitpoint signals. Requires `OPENROUTER_API_KEY` + local whisper.cpp.
- **[`retry-and-cancel`](examples/retry-and-cancel/)** -- minimal control-plane demo of retry-exhaustion terminal failure + `cancel_flow` cascade. No external dependencies beyond the running server.

Quick start with the coding agent:

```bash
# Terminal 1: worker
cd examples/coding-agent
OPENROUTER_API_KEY=sk-or-... OPENROUTER_MODEL=moonshotai/kimi-k2.6 cargo run --bin worker

# Terminal 2: submit a task
cd examples/coding-agent
cargo run --bin submit -- --issue "Write a Rust function that checks if a string is a palindrome"
```

## SDK claim_next() feature gate

The SDK's `claim_next()` method bypasses budget/quota admission control and is gated behind a feature flag. Enable it explicitly:

```toml
ff-sdk = { path = "crates/ff-sdk", features = ["direct-valkey-claim"] }
```

For production deployments, use the Scheduler (`ff-scheduler`) which enforces admission control before issuing claim grants. The direct claim path is intended for development, testing, and trusted single-worker setups.

## Crates

| Crate | Description |
|-------|-------------|
| `ferriskey` | Valkey client -- in-tree, forked from glide-core (valkey-glide). Hash-tag-aware `cluster_scan` single-shard routing. |
| `ff-core` | Core types, state enums, partition math, key builders, `EngineBackend` trait, error codes |
| `ff-script` | Typed FCALL wrappers and Lua library loader |
| `ff-engine` | Cross-partition dispatch and 17 background scanners |
| `ff-scheduler` | Claim-grant cycle, fairness, capability matching |
| `ff-backend-valkey` | `EngineBackend` implementation backed by Valkey FCALL |
| `ff-backend-postgres` | `EngineBackend` implementation backed by Postgres (RFC-017 Stage E) |
| `ff-backend-sqlite` | `EngineBackend` implementation backed by SQLite (RFC-023) — **dev-only**, gated behind `FF_DEV_MODE=1`. For `cargo test` without Docker and contributor onboarding. Not a deployment target. |
| `flowfabric` | Umbrella re-export crate; feature-flagged backend selection (`valkey` default, `postgres` opt-in) |
| `ff-sdk` | Worker SDK — public API for worker authors; includes the client-local `EngineBackendLayer` surface |
| `ff-server` | HTTP API server, Valkey connection, boot sequence, engine `/metrics` endpoint |
| `ff-observability` | OTEL + Prometheus instrumentation, optional Sentry integration |
| `ff-observability-http` | Consumer-side `/metrics` axum router for workers embedding `ff-sdk` in library mode |
| `ff-test` | Integration test harness, fixtures, assertion helpers |
| `ff-readiness-tests` | Release-gate behavioral smokes (lifecycle, flow fanout/join, HMAC roundtrip, cancel cascade) |

## Production considerations

- **Env var reference** — `ServerConfig::from_env` in `crates/ff-server/src/config.rs` is the source of truth. Full list below (required ones are marked `required`):

  | Variable | Default | Description |
  |----------|---------|-------------|
  | `FF_WAITPOINT_HMAC_SECRET` | *required* | Hex-encoded HMAC signing secret for waitpoint tokens (RFC-004 §Waitpoint Security). Even-length hex; 64 chars (32 bytes) recommended. |
  | `FF_BACKEND` | `valkey` | Backend family — `valkey`, `postgres`, or `sqlite`. Valkey + Postgres are first-class at v0.8.0 (RFC-017 Stage E4); SQLite lands at v0.12.0 as a **dev-only** backend (RFC-023). |
  | `FF_HOST` | `localhost` | Valkey host (ignored when `FF_BACKEND=postgres` or `sqlite`) |
  | `FF_PORT` | `6379` | Valkey port (ignored when `FF_BACKEND=postgres` or `sqlite`) |
  | `FF_TLS` | `false` | Enable Valkey TLS (`1` or `true`) |
  | `FF_CLUSTER` | `false` | Enable Valkey cluster mode |
  | `FF_POSTGRES_URL` | *(empty)* | Postgres connection URL (required when `FF_BACKEND=postgres`). Example: `postgres://user:pass@host:5432/db`. |
  | `FF_POSTGRES_POOL_SIZE` | `10` | Postgres pool size (ignored on non-Postgres paths). |
  | `FF_SQLITE_PATH` | `:memory:` | SQLite path or URI (required shape under `FF_BACKEND=sqlite`). File path (`/tmp/ff-dev.db`) or URI (`file:name?mode=memory&cache=shared`). Dev-only per RFC-023. |
  | `FF_SQLITE_POOL_SIZE` | `4` | SQLite pool size (1 writer + N–1 readers); ignored on non-SQLite paths. |
  | `FF_DEV_MODE` | *(unset)* | **Required** when `FF_BACKEND=sqlite` or when constructing `SqliteBackend::new` directly; server + backend refuse to start without it. Orthogonal to `FF_ENV=development` / `FF_BACKEND_ACCEPT_UNREADY=1`. No effect on Valkey / Postgres paths. See [`docs/dev-harness.md`](docs/dev-harness.md). |
  | `FF_LISTEN_ADDR` | `0.0.0.0:9090` | API listen address |
  | `FF_LANES` | `default` | Comma-separated lane names |
  | `FF_FLOW_PARTITIONS` | `256` | Flow partition count (exec keys co-locate under RFC-011) |
  | `FF_BUDGET_PARTITIONS` | `32` | Budget partition count |
  | `FF_QUOTA_PARTITIONS` | `32` | Quota partition count |
  | `FF_CORS_ORIGINS` | `*` | Comma-separated CORS origins; empty string is rejected |
  | `FF_API_TOKEN` | *(none)* | Shared-secret Bearer token; if set, all non-`/healthz` requests require it |
  | `FF_WAITPOINT_HMAC_GRACE_MS` | `86400000` | Grace window for previous-kid token acceptance after rotation |
  | `FF_MAX_CONCURRENT_STREAM_OPS` | `64` | Shared semaphore for stream reads + tails; legacy `FF_MAX_CONCURRENT_TAIL` still accepted |
  | `FF_LEASE_EXPIRY_INTERVAL_MS` | `1500` | Lease-expiry scanner interval |
  | `FF_DELAYED_PROMOTER_INTERVAL_MS` | `750` | Delayed-promoter scanner interval |
  | `FF_INDEX_RECONCILER_INTERVAL_S` | `45` | Index reconciler interval |
  | `FF_ATTEMPT_TIMEOUT_INTERVAL_S` | `2` | Attempt-timeout scanner interval |
  | `FF_SUSPENSION_TIMEOUT_INTERVAL_S` | `2` | Suspension-timeout scanner interval |
  | `FF_PENDING_WP_EXPIRY_INTERVAL_S` | `5` | Pending-waitpoint expiry interval |
  | `FF_RETENTION_TRIMMER_INTERVAL_S` | `60` | Retention-trimmer scanner interval |
  | `FF_BUDGET_RESET_INTERVAL_S` | `15` | Budget-reset scanner interval |
  | `FF_BUDGET_RECONCILER_INTERVAL_S` | `30` | Budget reconciler interval |
  | `FF_QUOTA_RECONCILER_INTERVAL_S` | `30` | Quota reconciler interval |
  | `FF_UNBLOCK_INTERVAL_S` | `5` | Unblock scanner interval |
  | `FF_DEPENDENCY_RECONCILER_INTERVAL_S` | `15` | DAG dependency reconciler interval |
  | `FF_FLOW_PROJECTOR_INTERVAL_S` | `15` | Flow projector scanner interval |
  | `FF_EXECUTION_DEADLINE_INTERVAL_S` | `5` | Execution-deadline scanner interval |
  | `FF_CANCEL_RECONCILER_INTERVAL_S` | `15` | Cancel-reconciler scanner interval |
- **Auth is opt-in** — if `FF_API_TOKEN` is unset, every endpoint except `GET /healthz` is reachable without authentication. Always set `FF_API_TOKEN` (and consider CORS via `FF_CORS_ORIGINS`) before exposing the server beyond localhost.
- **No API rate limiting** — deploy behind a reverse proxy (nginx, Envoy) with rate limiting configured
- **No HTTP connection limit** — axum accepts unbounded connections; configure limits at the load balancer
- **cancel_flow** returns `CancellationScheduled` immediately for `cancel_all` policy and dispatches member cancellations asynchronously (see `POST /v1/flows/{id}/cancel`); append `?wait=true` for synchronous completion. Background dispatch is drained with a 15s timeout on shutdown and may be aborted for very large flows.
- **detect_cycle BFS** can issue ~100K `redis.call` operations on large DAGs (up to 1000 nodes × edges per node), blocking the Valkey event loop; keep flow graph fan-out moderate
- **Terminal operation replay** — `complete()`, `fail()`, and `cancel()` are idempotent on retry: if the connection drops after the Lua commit but before the client reads the response, a retry by the same caller (matching `lease_epoch` + `attempt_id`) is reconciled by the SDK and returns the same outcome as the original call would have. A retry after a different terminal op committed (e.g. cancel after complete) surfaces as `ExecutionNotActive` with populated `terminal_outcome` so the caller can inspect what actually happened.
- **cancel_flow dispatch durability** — the async member-cancel loop is durable against process crashes and permanent per-member errors via a per-flow-partition `cancel_backlog` ZSET and per-flow `pending_cancels` SET. Live dispatch acks members as they cancel; the `cancel_reconciler` scanner drains any remainder on its interval (default 15s). A worker can still claim a cancelled flow's member in the cross-slot window between `cancel_flow` and the member's own `ff_cancel_execution` — `ff_claim_execution` reads exec_core on `{p:N}` and cannot atomically consult `flow_core.public_flow_state` on `{fp:N}`, so a member may run one partial attempt before the reconciler cancels it. `?wait=true` still eliminates that window synchronously; default-async now converges within `reconciler_interval + grace_ms` rather than relying on retention.
- **Lua library after failover** — the server auto-reloads the Lua library on first "function not found" error after a Valkey failover, but the first request in the new epoch will fail and be retried

### Rolling upgrades

KEYS-arity changes (and the `LIBRARY_VERSION` bump that paired with Batch A) require **blue-green or stop-then-start deployment**. Running old and new `ff-server` binaries against the same Valkey simultaneously can produce Lua errors on the old binary because the new binary loads a library whose KEYS arity differs from what the old binary expects — `redis.call(..., nil, ...)` surfaces as `ResponseError`, not silent corruption, but requests will fail until the old instances are drained.

The version string lives in **`lua/version.lua`** (single source of truth). `scripts/gen-ff-script-lua.sh` extracts it and writes `crates/ff-script/src/flowfabric_lua_version`, which Rust reads via `include_str!`. Regenerate after any edit to `lua/*.lua`; CI fails on drift.

Future: per-FCALL version negotiation. For now: drain old instances before starting new ones, or run a single writer at a time.

## CI coverage

Two PR-time GitHub Actions workflows gate merges:

- **`.github/workflows/matrix.yml`** — 6-job host × valkey × mode matrix. RFC-011 §13 requires Valkey >= 7.2, enforced at `ff-server` boot.
  - linux x86_64 (`ubuntu-latest`) × valkey 8 (`valkey/valkey:8-alpine`) × {standalone, cluster}
  - linux x86_64 (`ubuntu-latest`) × valkey 7.2 (`valkey/valkey:7.2`) × standalone — guards against accidental 8-specific primitive adoption
  - linux arm64 (`ubuntu-24.04-arm`) × valkey 8 × {standalone, cluster}
  - macos arm64 (`macos-latest`) × valkey 8 × standalone (Homebrew Valkey, currently tracking the 8.x line unpinned; docker-on-mac on GitHub hosted runners does not support the privileged-mode cluster setup).
  - Cluster mode on linux uses a 3-master 3-replica cluster via `.github/cluster/docker-compose.cluster.yml` + `bootstrap.sh`.
- **`.github/workflows/security-and-quality.yml`** — 6 independent gates, any one blocks merge:
  - `cargo audit` with `--deny warnings`; ignore list in `.cargo/audit.toml`
  - `cargo deny check` (licenses, bans, sources, advisories); policy in `deny.toml`
  - `cargo geiger` unsafe-expression ratchet against a committed baseline at `.github/geiger-baseline.json`. Refresh with `bin/update-geiger-baseline.sh` in a PR that deliberately changes the unsafe count.
  - CodeQL gate (polls `codeql.yml` for the same SHA)
  - `cargo machete` unused-dep detection on `crates/` + `ferriskey` (benches and examples are out of scope — their authors own hygiene there)
  - `cargo semver-checks` against the latest `v*.*.*` git tag; skips gracefully before the first release.

`cargo install` takes a minute per tool; the `Swatinem/rust-cache@v2` key partitioning in both workflows amortises this across PR pushes.

## License

Apache-2.0
