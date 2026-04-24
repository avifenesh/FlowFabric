# Consumer Migration Guide — FlowFabric v0.8.0

RFC-017 Wave 8 ships the Postgres backend as a first-class
`EngineBackend` over the full HTTP surface and retires the v0.7-era
compatibility shims. This guide covers the breaking changes a
library-mode consumer (cairn-fabric or any out-of-tree embedder) must
adapt to.

> Audience: crates that depend on `ff-sdk`, `ff-server`, `ff-core`,
> or `ff-engine` directly. Pure HTTP API consumers only need the
> `PendingWaitpointInfo` section.

## 1. `ServerConfig` flat Valkey fields removed

**Before (v0.7.x):**

```rust
use ff_server::config::ServerConfig;

let cfg = ServerConfig {
    host: "valkey-0.internal".into(),
    port: 6379,
    tls: true,
    cluster: true,
    skip_library_load: false,
    // ... other fields ...
    ..ServerConfig::default()
};
```

**After (v0.8.0):**

```rust
use ff_server::config::{ServerConfig, ValkeyServerConfig, BackendKind};

let cfg = ServerConfig {
    backend: BackendKind::Valkey,
    valkey: ValkeyServerConfig {
        host: "valkey-0.internal".into(),
        port: 6379,
        tls: true,
        cluster: true,
        skip_library_load: false,
    },
    // ... other fields ...
    ..ServerConfig::default()
};
```

The `valkey: ValkeyServerConfig` field mirrors the pre-existing
`postgres: PostgresServerConfig` and is ignored when
`backend == BackendKind::Postgres`. Env-var loading via
`ServerConfig::from_env()` is unchanged (`FF_HOST` / `FF_PORT` / etc.
still read into `cfg.valkey.*`).

`ValkeyServerConfig` is **not** `#[non_exhaustive]` — you can
construct the struct literal directly. Adding a field is a v0.y.0
breaking bump; insulate with `..ValkeyServerConfig::default()` if you
prefer.

## 2. `PendingWaitpointInfo` wire-format break

`GET /v1/executions/{id}/pending-waitpoints` no longer includes a raw
`waitpoint_token` HMAC in each entry. Clients must correlate
waitpoints by `(token_kid, token_fingerprint)`.

**Before (v0.7.x):**

```json
{
  "entries": [
    {
      "execution_id": "...",
      "waitpoint_id": "...",
      "token_kid": "v1",
      "token_fingerprint": "sha256:...",
      "waitpoint_token": "v1.eyJhbGciOi...",
      "...": "..."
    }
  ]
}
```

Response also carried a `Deprecation: ff-017` header and bumped the
`ff_pending_waitpoint_legacy_token_served_total` counter.

**After (v0.8.0):**

```json
[
  {
    "execution_id": "...",
    "waitpoint_id": "...",
    "token_kid": "v1",
    "token_fingerprint": "sha256:...",
    "...": "..."
  }
]
```

The top-level response is now the sanitised `PendingWaitpointInfo`
array directly (no `{"entries": [...]}` wrapper — note this was the
v0.7-era shape; v0.8.0 confirms the unwrapped list). The
`Deprecation` header is gone. If your code was re-signing or
verifying the `waitpoint_token` client-side, switch to deriving the
same logical identity from `(token_kid, token_fingerprint)`.

The Rust type `ff_core::PendingWaitpointInfo` no longer carries a
`waitpoint_token` field. The Valkey-only inherent
`Server::fetch_waitpoint_token_v07` + `EngineBackend` trait method of
the same name are removed.

## 3. `FF_BACKEND=postgres` setup

Postgres boots natively at v0.8.0 — no `FF_BACKEND_ACCEPT_UNREADY=1` /
`FF_ENV=development` escape hatch required.

### Required env vars

| Env var | Default | Notes |
|---|---|---|
| `FF_BACKEND` | `valkey` | Set to `postgres` to boot the Postgres path. |
| `FF_POSTGRES_URL` | *(empty)* | Required on the Postgres path. `postgres://user:pass@host:port/db` shape (libpq / sqlx). |
| `FF_POSTGRES_POOL_SIZE` | `10` | Max pool connections; ignored on Valkey path. |
| `FF_WAITPOINT_HMAC_SECRET` | *(empty)* | Required on both paths. |

### Migrations

`ff-server` auto-runs `apply_migrations` at boot against
`FF_POSTGRES_URL`. The initial schema (Waves 3 + 3b) seeds the full
execution / flow / attempt / budget tableset. Re-runs are idempotent.

### Parity

See [`docs/POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md) for
the per-method status at v0.8.0. At release time, the Postgres path
ships:

- Full create/read-ingress parity (create_flow, create_execution,
  add_execution_to_flow, stage_dependency_edge,
  apply_dependency_to_child).
- Full flow family (describe_flow, list_flows, list_edges,
  describe_edge, cancel_flow, set_edge_group_policy).
- Scheduler + `claim_for_worker` + 6 reconcilers.
- `ping`, `backend_label`, `shutdown_prepare`.

Rows marked `stub` on the Postgres column return
`EngineError::Unavailable { op }` → HTTP 503 with a structured body:

- operator control (`cancel_execution`, `change_priority`,
  `replay_execution`, `revoke_lease`)
- read model (`read_execution_info`, `read_execution_state`,
  `get_execution_result`)
- budget / quota admin
- `list_pending_waitpoints`
- `rotate_waitpoint_hmac_secret_all`

These land in Wave 9. If your consumer needs any of them against
Postgres before Wave 9 ships, stay on the Valkey backend for now.

## 4. `EngineBackend` trait expansion (custom backend impls)

If you implement `EngineBackend` for a bespoke backend (test doubles,
alternate storage), the trait grew by ~17 methods between Wave 4 and
Wave 8. All new methods carry sensible defaults:

- Most return `Err(EngineError::Unavailable { op: "<name>" })`.
- `backend_label` returns `"custom"` if unset.
- `ping` returns `Ok(())` if unset.
- `shutdown_prepare` returns `Ok(())` if unset.

The `HookedBackend`, `PassthroughBackend`, `ValkeyBackend`, and
`PostgresBackend` impls in-tree cover every method concretely and are
the reference for what a "complete" impl looks like.

## 5. Other v0.8.0 breakage (v0.7-cycle rollups)

- **`ClaimPolicy`** — `ClaimPolicy::immediate()` removed. Constructor
  is now `ClaimPolicy::new(worker_id, worker_instance_id,
  lease_ttl_ms, max_wait)`.
- **`ReclaimToken`** — `ReclaimToken::new` gained `worker_id`,
  `worker_instance_id`, `lease_ttl_ms` args to match.
- **`ContentionKind::RetryExhausted`** — additive variant on the
  `#[non_exhaustive]` enum. Consumer match-arms on `Contention(_)`
  keep working; specific-variant matches may need an update.
- **`ServerError`** — additive `#[non_exhaustive]` variants across
  Stages D1–E3. Catch-all arm required.

## 6. Removed server API

- `Server::client()` accessor / `Server::client` field — the Server
  no longer exposes an ambient Valkey `Client`. Consumers that need
  Valkey connectivity should build their own `ferriskey::ClientBuilder`
  using the same `ServerConfig::valkey` parameters, or go through the
  `EngineBackend` trait.
- `Server::fcall_with_reload` — inlined into the Valkey backend impl.
- `Server::scheduler` field — replaced by trait-routed dispatch on
  `backend.claim_for_worker`.

## 7. Upgrading

1. Bump `ff-*` deps to `0.8.0`.
2. Update `ServerConfig` construction sites (section 1).
3. Drop `waitpoint_token` from your `/v1/pending-waitpoints`
   deserialiser (section 2).
4. If you match on `ClaimPolicy::immediate()`, adjust (section 5).
5. Add a catch-all arm to any `match` on `ServerError`.
6. If migrating to Postgres, set `FF_BACKEND=postgres` +
   `FF_POSTGRES_URL` + `FF_WAITPOINT_HMAC_SECRET` (section 3).
7. Re-run your integration smoke. File issues against
   [`rfcs/RFC-017-ff-server-backend-abstraction.md`](../rfcs/RFC-017-ff-server-backend-abstraction.md)
   for any migration-guide gap you hit.
