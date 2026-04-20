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

### 1. Start Valkey

```bash
docker run -d --name valkey -p 6379:6379 valkey/valkey:7.2
```

### 2. Start the FlowFabric server

```bash
cargo run -p ff-server
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

- **Full env var reference** — see the `ServerConfig::from_env` rustdoc in `crates/ff-server/src/config.rs` for the complete table (FF_HOST, FF_PORT, FF_TLS, FF_CLUSTER, FF_LANES, FF_CORS_ORIGINS, FF_API_TOKEN, FF_WAITPOINT_HMAC_SECRET (required), FF_WAITPOINT_HMAC_GRACE_MS, FF_MAX_CONCURRENT_STREAM_OPS, scanner intervals, partition counts).
- **Auth is opt-in** — if `FF_API_TOKEN` is unset, every endpoint except `GET /healthz` is reachable without authentication. Always set `FF_API_TOKEN` (and consider CORS via `FF_CORS_ORIGINS`) before exposing the server beyond localhost.
- **No API rate limiting** — deploy behind a reverse proxy (nginx, Envoy) with rate limiting configured
- **No HTTP connection limit** — axum accepts unbounded connections; configure limits at the load balancer
- **cancel_flow** returns `CancellationScheduled` immediately for `cancel_all` policy and dispatches member cancellations asynchronously (see `POST /v1/flows/{id}/cancel`); append `?wait=true` for synchronous completion. Background dispatch is drained with a 15s timeout on shutdown and may be aborted for very large flows.
- **detect_cycle BFS** can issue ~100K `redis.call` operations on large DAGs (up to 1000 nodes × edges per node), blocking the Valkey event loop; keep flow graph fan-out moderate
- **Terminal operation ambiguity** — if the Valkey connection drops during `complete()`, `fail()`, or `cancel()`, the operation may have committed server-side; callers should verify execution state via `get_execution_state()` before retrying
- **cancel_flow dispatch-drop race** — the async member-cancel loop can drop individual cancellations under a transient Valkey error. `ff_claim_execution` reads exec_core on `{p:N}` and cannot atomically consult `flow_core.public_flow_state` on `{fp:N}` (cross-slot), so a worker may still claim and complete a member of a cancelled flow. The flow itself is terminal; only the one member escapes. Use `?wait=true` when synchronous cancellation matters, or rely on retention to trim the stale member
- **Lua library after failover** — the server auto-reloads the Lua library on first "function not found" error after a Valkey failover, but the first request in the new epoch will fail and be retried

### Rolling upgrades

KEYS-arity changes (and the `LIBRARY_VERSION` bump that paired with Batch A) require **blue-green or stop-then-start deployment**. Running old and new `ff-server` binaries against the same Valkey simultaneously can produce Lua errors on the old binary because the new binary loads a library whose KEYS arity differs from what the old binary expects — `redis.call(..., nil, ...)` surfaces as `ResponseError`, not silent corruption, but requests will fail until the old instances are drained.

The version string lives in **`lua/version.lua`** (single source of truth). `scripts/gen-ff-script-lua.sh` extracts it and writes `crates/ff-script/src/flowfabric_lua_version`, which Rust reads via `include_str!`. Regenerate after any edit to `lua/*.lua`; CI fails on drift.

Future: per-FCALL version negotiation. For now: drain old instances before starting new ones, or run a single writer at a time.

## CI coverage

Two PR-time GitHub Actions workflows gate merges:

- **`.github/workflows/matrix.yml`** — 10-job host × Valkey × mode matrix:
  - linux x86_64 (`ubuntu-latest`) and linux arm64 (`ubuntu-24.04-arm`) × Valkey 7.2 + latest × {standalone, cluster}
  - mac arm64 (`macos-latest`) and mac x86_64 (`macos-13`) × Valkey latest × standalone
  - Linux covers the full version matrix where we deploy. Mac runners validate cross-arch Rust correctness against the latest Valkey release only (Homebrew does not package 7.2 and docker-on-mac on GitHub hosted runners does not support the privileged-mode cluster setup).
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
