# FlowFabric deployment guide

Minimum-viable production deployment. Covers the four components you need:
one `ff-server`, one Valkey, N workers linked against `ff-sdk`, and a
reverse proxy for TLS + rate limiting. FlowFabric is pre-1.0; treat this
as guidance, not a contract.

> For local development use `cargo run -p ff-server` with
> `FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32)` — the README's
> quickstart section is simpler than this guide.

## Topology

```
             ┌──────────────┐
             │ reverse proxy│  TLS, rate limit, (optional) auth mirror
             │  (nginx /    │
             │   Envoy)     │
             └──────┬───────┘
                    │ HTTP (plaintext, localhost or private net)
                    ▼
             ┌──────────────┐
             │  ff-server   │  :9090 (axum) — stateless, horizontally
             │ (1+ replica) │  scalable behind the proxy
             └──────┬───────┘
                    │ RESP (optionally TLS)
                    ▼
             ┌──────────────┐
             │    Valkey    │  :6379 — the source of truth
             │  (7.2 / 8)   │  standalone or cluster
             └──────▲───────┘
                    │ RESP (workers talk directly for claim/signal)
                    │
             ┌──────┴───────┐
             │   workers    │  N processes linking ff-sdk
             │ (your code)  │
             └──────────────┘
```

Workers open their own Valkey connection via the SDK; they do **not** route
execution traffic through `ff-server`. Keep this in mind when sizing the
Valkey connection budget.

## 1. Valkey

FlowFabric targets [Valkey](https://valkey.io/) 7.2+. CI exercises 7.2 and 8.
Redis 7.x works in practice today but is not part of the support matrix
(module ABI and behaviour diverge post-fork).

### Standalone vs cluster

| Mode        | When to pick it                                                                 |
| ----------- | ------------------------------------------------------------------------------- |
| Standalone  | Development, single-tenant, throughput fits one node (< ~50k ops/s). Simplest.  |
| Cluster     | Horizontal write-scale needed, operational experience with cluster ops on hand. |

FlowFabric hash-tags every key — `{fp:N}` for flow + execution (RFC-011
co-locates both families on the same tag), `{b:M}` for budgets, `{q:K}`
for quotas — so related keys always land on the same slot. Increasing
`FF_FLOW_PARTITIONS` gives the cluster more slots to spread across
without changing the model.

### Persistence

Valkey defaults (RDB snapshots only) are **not** safe for FlowFabric
state. Turn on AOF with fsync-every-second or stronger:

```conf
# valkey.conf
appendonly yes
appendfsync everysec
# Keep RDB for fast restarts:
save 900 1
save 300 10
save 60 10000
# Rewrite AOF in the background when it doubles:
auto-aof-rewrite-percentage 100
auto-aof-rewrite-min-size 64mb
```

### Memory sizing

Rough planning numbers (production, steady state):

- Baseline overhead: ~50 MB per empty node.
- Per in-flight attempt: ~4 KB (`exec_core` hash + indexes + stream frames
  until trimmed by the retention scanner).
- Per completed-but-retained attempt: ~1 KB until
  `FF_RETENTION_TRIMMER_INTERVAL_S` reclaims it.
- Budget / quota counters: negligible (tens of bytes each).

For 10k concurrent attempts with a 1-hour retention window and ~20k
completions/hour, plan for roughly 200 MB + headroom. Set `maxmemory`
explicitly and pick an eviction policy that refuses writes rather than
evicting state:

```conf
maxmemory 2gb
maxmemory-policy noeviction
```

`noeviction` is mandatory — silent key eviction corrupts the scheduler's
invariants.

### Security

- Bind to a private interface, or use `requirepass` + TLS.
- TLS between `ff-server` and Valkey: set `FF_TLS=true` and point
  `FF_HOST` at the TLS endpoint. The client uses rustls defaults
  (platform trust store).

## 2. ff-server

Stateless HTTP API in front of Valkey. Runs the 15 background scanners
in `ff-engine` (attempt-timeout, budget-reset, budget-reconciler,
cancel-reconciler, delayed-promoter, dependency-reconciler,
execution-deadline, flow-projector, index-reconciler, lease-expiry,
pending-waitpoint-expiry, quota-reconciler, retention-trimmer,
suspension-timeout, unblock) plus an optional completion listener —
they drive timeouts, promotions, reconciliation, and retention.

### Required env vars

| Variable                   | Purpose                                                                                               |
| -------------------------- | ----------------------------------------------------------------------------------------------------- |
| `FF_WAITPOINT_HMAC_SECRET` | Even-length hex signing secret for waitpoint tokens (RFC-004). **Server refuses to start without it.** 64 hex chars recommended. Source from a secret manager. |

### Recommended env vars for any non-trivial deployment

| Variable          | Typical value                          | Why                                                                         |
| ----------------- | -------------------------------------- | --------------------------------------------------------------------------- |
| `FF_HOST`         | Valkey hostname                        | Defaults to `localhost` — almost always wrong in production.                |
| `FF_PORT`         | `6379`                                 | Set if you moved Valkey off the default port.                               |
| `FF_TLS`          | `true`                                 | TLS to Valkey whenever the link crosses a trust boundary.                   |
| `FF_CLUSTER`      | `true` when using Valkey cluster.      | Enables cluster-aware routing in the embedded client.                       |
| `FF_LISTEN_ADDR`  | `0.0.0.0:9090` or `127.0.0.1:9090`     | Bind to loopback if a reverse proxy is on the same host.                    |
| `FF_API_TOKEN`    | Random 32+ byte secret, base64.        | Without it, every route except `GET /healthz` is unauthenticated.           |
| `FF_CORS_ORIGINS` | Comma-separated allowlist, or `*`      | Default is permissive (`*`). Tighten before exposing the API to browsers.   |

### Optional: Sentry error reporting

Enable the `sentry` feature on `ff-server` (or a consumer binary that
depends on `ff-observability`) to ship `tracing::error!` events and
panics to a Sentry project. All configuration is via `FF_SENTRY_*` env
vars; leaving `FF_SENTRY_DSN` unset is a graceful no-op (no network, no
background thread). See `ff-observability::sentry` rustdoc for the full
contract.

| Variable                | Default                  | Purpose                                                          |
| ----------------------- | ------------------------ | ---------------------------------------------------------------- |
| `FF_SENTRY_DSN`         | unset (disables)         | Sentry project DSN. Required to activate.                        |
| `FF_SENTRY_ENVIRONMENT` | `production`             | Sentry `environment` tag (`staging`, `dev`, …).                  |
| `FF_SENTRY_RELEASE`     | crate `CARGO_PKG_VERSION`| Sentry `release` tag — wire in a git SHA or build ID here.       |

The complete list of variables (all the scanner intervals, partition
counts, etc.) is in the rustdoc for
[`ServerConfig::from_env`](../crates/ff-server/src/config.rs) and mirrored
in the project `README.md`. Both tables are kept in sync manually; the
`from_env` rustdoc is the canonical reference.

### Ports

- `9090/tcp` — HTTP API (axum). Plaintext. Put it behind a proxy if it
  crosses a trust boundary.
- `GET /healthz` is unauthenticated and cheap — use it for liveness
  probes.

### Scaling ff-server horizontally

`ff-server` is effectively stateless: state lives in Valkey. You can run
multiple replicas behind the proxy. The background scanners are
cooperative — running more than one replica is safe but does not speed up
scanner work, since each scanner claims its partition shards via Lua.
Two to three replicas is a reasonable HA target; going wider wastes
cycles without changing throughput.

## 3. Workers (ff-sdk)

Workers are your code. They link against `ff-sdk`, declare capabilities,
and process attempts claimed from the scheduler.

### Sizing and placement

- Start with one worker per capability class per host. Scale out
  horizontally; workers are stateless.
- `worker_id` **must be unique** across the deployment. Any string works;
  `${hostname}-${pid}` or a UUID are both fine. Reusing a `worker_id`
  after a crash is safe — the scheduler reclaims the previous lease via
  `FF_LEASE_EXPIRY_INTERVAL_MS`.

### Lease TTL tuning

`lease_ttl_ms` (per-claim, set in the SDK call) controls how long a
claim survives without a heartbeat.

| Workload                 | Suggested `lease_ttl_ms` |
| ------------------------ | ------------------------ |
| Sub-second operations    | 5_000                    |
| Seconds-to-minutes       | 30_000 – 60_000          |
| Long-running (LLM, CI)   | 300_000 with heartbeats  |

Rules of thumb:

- Lease TTL must be longer than any non-heartbeat gap in your worker.
- Longer leases slow crash recovery — a dead worker blocks retries until
  the lease expires.
- Heartbeats (`heartbeat_attempt`) refresh the lease without changing
  its original TTL, so set the TTL once at claim time.

### Capability declarations

Each worker declares the capability set it can serve (see the
coding-agent example under `examples/coding-agent/` for the canonical
pattern). Capabilities route work; keep them deliberate and narrow.

### Feature gates

`ff-sdk`'s direct `claim_next()` bypasses budget/quota admission and is
gated behind the `direct-valkey-claim` feature. Production deployments
use `ff-scheduler`'s claim-grant cycle instead. Enable the direct path
only for dev/test or trusted single-worker setups — see the README for
the toggle.

## 4. Reverse proxy (TLS + rate limit)

`ff-server` serves plaintext HTTP and does not rate-limit. Both jobs
belong to a proxy in front of it — nginx, Envoy, HAProxy, or a managed
LB work.

Minimum expectations for the proxy:

- Terminate TLS.
- Enforce rate limits per client. `ff-server` has no HTTP connection cap
  either (axum accepts unbounded connections), so the proxy is also the
  connection-limit point.
- Forward `Authorization: Bearer <FF_API_TOKEN>` unmodified.
- `GET /healthz` should be reachable without auth for probes.

Example nginx snippet:

```nginx
upstream ff_server {
    server 127.0.0.1:9090;
    keepalive 32;
}

limit_req_zone $binary_remote_addr zone=ff_api:10m rate=100r/s;

server {
    listen 443 ssl http2;
    server_name flowfabric.example.com;

    ssl_certificate     /etc/ssl/certs/flowfabric.pem;
    ssl_certificate_key /etc/ssl/private/flowfabric.key;

    location = /healthz {
        proxy_pass http://ff_server;
    }

    location / {
        limit_req zone=ff_api burst=200 nodelay;
        proxy_pass http://ff_server;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

## 5. Docker Compose reference

A minimum stack you can adapt. This runs Valkey with AOF, one
`ff-server`, and leaves worker processes to your application image.

```yaml
# docker-compose.yml
services:
  valkey:
    image: valkey/valkey:8-alpine
    command: >
      valkey-server
      --appendonly yes
      --appendfsync everysec
      --maxmemory 2gb
      --maxmemory-policy noeviction
    volumes:
      - valkey-data:/data
    healthcheck:
      test: ["CMD", "valkey-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 5

  ff-server:
    image: flowfabric/ff-server:latest  # build from this repo
    depends_on:
      valkey:
        condition: service_healthy
    environment:
      FF_HOST: valkey
      FF_PORT: "6379"
      FF_LISTEN_ADDR: "0.0.0.0:9090"
      FF_WAITPOINT_HMAC_SECRET: ${FF_WAITPOINT_HMAC_SECRET:?set via env or secret}
      FF_API_TOKEN: ${FF_API_TOKEN:?set via env or secret}
      FF_FLOW_PARTITIONS: "256"
      RUST_LOG: "info,ff_server=info,ff_engine=info"
    ports:
      - "127.0.0.1:9090:9090"  # bind to loopback; proxy handles :443
    restart: unless-stopped

  # Example worker — replace with your own image.
  worker:
    image: yourorg/your-worker:latest
    depends_on:
      - ff-server
    environment:
      FF_HOST: valkey
      FF_PORT: "6379"
      FF_API_TOKEN: ${FF_API_TOKEN:?set via env or secret}
      WORKER_ID: "${HOSTNAME}-worker-1"
    restart: unless-stopped
    deploy:
      replicas: 4

volumes:
  valkey-data:
```

Generate the HMAC secret and API token once, out-of-band:

```bash
export FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32)
export FF_API_TOKEN=$(openssl rand -base64 32)
```

## 6. Observability

Today `ff-server` emits structured `tracing` logs to stderr; route them
with `RUST_LOG` (e.g. `RUST_LOG=info,ff_engine=debug`). There is no
Prometheus endpoint yet — metrics are tracked as
[issue #94](https://github.com/HiveTechs/FlowFabric/issues/94). Until
that lands, scrape log-derived metrics (e.g. via Vector or Loki) and
use `GET /healthz` for liveness.

Log lines worth alerting on:

- `error` at `ff_server::server` — a request failed to complete.
- `warn` containing `lease expired` — indicates workers dying or leases
  set too short.
- `warn` at `ff_engine::*` reconcilers — scanner-level errors;
  occasional ones are normal, sustained ones mean Valkey pressure.

## 7. Upgrade checklist

1. Drain workers gracefully (stop claiming, finish in-flight attempts).
2. Stop `ff-server` replicas; new versions will auto-reload the Lua
   library on first use.
3. Roll the new `ff-server` image.
4. Start workers against the new server.
5. Confirm `GET /healthz` is 200 and scanner intervals in the logs
   match expectations.

Rolling upgrades work when the release notes say so; otherwise treat
every upgrade as stop-the-world until FlowFabric ships a documented
rolling path.
