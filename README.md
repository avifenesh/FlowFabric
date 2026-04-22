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
          │  14 scanners │ │ worker  │ │ claim-grant   │
          └──────┬──────┘ │   API   │ │   cycle       │
                 │        └────┬────┘ └───────┬───────┘
                 │             │              │
                 └──────────────┼──────────────┘
                                │
                         ┌──────┴──────┐
                         │  ff-script  │  typed FCALL wrappers
                         └──────┬──────┘
                                │
                         ┌──────┴──────┐
                         │   ff-core   │  types, state, keys, errors
                         └──────┬──────┘
                                │
                         ┌──────┴──────┐
                         │  ferriskey  │  Valkey client (Rust)
                         └─────────────┘
```

## Features

- **Lease-based ownership** -- workers hold leases on executions; auto-renewed, crash-safe
- **Suspend / signal / resume** -- human-in-the-loop and async event-driven workflows
- **Flow coordination** -- DAG execution with dependency edges, fan-out, skip propagation
- **Budget and quota** -- per-dimension usage tracking, hard/soft limits, sliding-window rate limiting
- **Streaming output** -- append-only frame streams scoped to each attempt
- **Priority scheduling** -- score-based eligible sets with priority clamping
- **REST API** -- 22 endpoints on axum with JSON error handling, CORS, health check
- **Cluster-safe** -- all operations use hash-tag partitioning, no SCAN, no CrossSlot

## Quick start

### 1. Start Valkey 8

FlowFabric requires Valkey >= 8.0 (RFC-011 §13, enforced at boot). Valkey 7.x
is rejected by `ff-server`.

```bash
docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
```

### 2. Start the FlowFabric server

`FF_WAITPOINT_HMAC_SECRET` is required at boot — the server refuses to start
without it so waitpoint HMAC authentication can never be silently disabled
(see RFC-004 §Waitpoint Security). Any even-length hex string works for
local dev; production should use 64 hex chars (32 bytes) from a secret
manager.

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) cargo run -p ff-server
```

### 3. Try the coding-agent example

See [examples/coding-agent/](examples/coding-agent/) for a full working example with an LLM-powered worker, human-in-the-loop review, and CLI tooling.

```bash
# Terminal 1: worker
cd examples/coding-agent
OPENROUTER_API_KEY=sk-or-... cargo run --bin worker

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
| `ferriskey` | Valkey client -- forked from glide-core (valkey-glide) |
| `ff-core` | Core types, state enums, partition math, key builders, error codes |
| `ff-script` | Typed FCALL wrappers and Lua library loader |
| `ff-engine` | Cross-partition dispatch and 14 background scanners |
| `ff-scheduler` | Claim-grant cycle, fairness, capability matching |
| `ff-sdk` | Worker SDK -- public API for worker authors |
| `ff-server` | HTTP API server, Valkey connection, boot sequence |
| `ff-test` | Integration test harness, fixtures, assertion helpers |

## Production considerations

- **Env var reference** — `ServerConfig::from_env` in `crates/ff-server/src/config.rs` is the source of truth. Full list below (required ones are marked `required`):

  | Variable | Default | Description |
  |----------|---------|-------------|
  | `FF_WAITPOINT_HMAC_SECRET` | *required* | Hex-encoded HMAC signing secret for waitpoint tokens (RFC-004 §Waitpoint Security). Even-length hex; 64 chars (32 bytes) recommended. |
  | `FF_HOST` | `localhost` | Valkey host |
  | `FF_PORT` | `6379` | Valkey port |
  | `FF_TLS` | `false` | Enable TLS (`1` or `true`) |
  | `FF_CLUSTER` | `false` | Enable cluster mode |
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

- **`.github/workflows/matrix.yml`** — 5-job host × mode matrix against Valkey 8 (`valkey/valkey:8-alpine`). RFC-011 §13 requires Valkey >= 8.0, enforced at `ff-server` boot; the pre-RFC-011 Valkey 7.2 rows are retired.
  - linux x86_64 (`ubuntu-latest`) × {standalone, cluster}
  - linux arm64 (`ubuntu-24.04-arm`) × {standalone, cluster}
  - macos arm64 (`macos-latest`) × standalone (Homebrew Valkey 8; docker-on-mac on GitHub hosted runners does not support the privileged-mode cluster setup).
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
