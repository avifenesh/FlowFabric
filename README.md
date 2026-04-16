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
- **REST API** -- 20 endpoints on axum with JSON error handling, CORS, health check
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

- **No API rate limiting** — deploy behind a reverse proxy (nginx, Envoy) with rate limiting configured
- **No HTTP connection limit** — axum accepts unbounded connections; configure limits at the load balancer
- **cancel_flow on large flows** blocks the API handler while sequentially cancelling each member execution; consider async fan-out for flows with >1K members
- **detect_cycle BFS** can issue ~100K `redis.call` operations on large DAGs (up to 1000 nodes × edges per node), blocking the Valkey event loop; keep flow graph fan-out moderate
- **Terminal operation ambiguity** — if the Valkey connection drops during `complete()`, `fail()`, or `cancel()`, the operation may have committed server-side; callers should verify execution state via `get_execution_state()` before retrying
- **Lua library after failover** — the server auto-reloads the Lua library on first "function not found" error after a Valkey failover, but the first request in the new epoch will fail and be retried

## License

Apache-2.0
