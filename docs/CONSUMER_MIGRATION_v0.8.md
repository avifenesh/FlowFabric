# Consumer Migration Guide — FlowFabric v0.8.0 → v0.9.0

RFC-017 Wave 8 (v0.8.0) ships the Postgres backend as a first-class
`EngineBackend` over the full HTTP surface and retires the v0.7-era
compatibility shims. **v0.9.0 is a fully additive release on top of
v0.8** — no v0.8 → v0.9 migration work required. This guide covers
the breaking changes a library-mode consumer (cairn-fabric or any
out-of-tree embedder) must adapt to when moving from v0.7, plus the
v0.9 additions that let you delete adapter code.

> Audience: crates that depend on `ff-sdk`, `ff-server`, `ff-core`,
> or `ff-engine` directly. Pure HTTP API consumers only need the
> `PendingWaitpointInfo` section.

## v0.9 additions (additive — no migration required)

Each of these lets you delete adapter code. None are required to
build against v0.9; existing v0.8 consumer code compiles unchanged.

| Addition | Replaces | Issue |
|---|---|---|
| `flowfabric` umbrella crate | 7 ff-* `Cargo.toml` pins | #279 |
| `ff-sdk` re-exports `ClaimGrant` + claim types | Direct `ff-scheduler` pin for type-only import | #283 |
| `LeaseSummary` adds `lease_id` + `attempt_index` + `last_heartbeat_at` | Consumer HGET wrapper | #278 |
| `EngineBackend::seed_waitpoint_hmac_secret` | Raw HSET boot path | #280 |
| `EngineBackend::prepare` | `ff_script::loader::ensure_library` + `BackendKind::Valkey` branch | #281 |

See §8 "Umbrella crate (flowfabric)" for the most load-bearing
change — replaces 7 pinned dependencies with one.

---

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

## 8. Umbrella crate (`flowfabric`)

Added in v0.8.2 per issue [#279](https://github.com/avifenesh/FlowFabric/issues/279).
Consumers who were pinning the 7-crate ff-* family in lockstep can
switch to a single `flowfabric` pin and let Cargo resolve a coherent
set.

### Before

```toml
[dependencies]
ff-core             = "0.8"
ff-sdk              = "0.8"
ff-engine           = "0.8"
ff-scheduler        = "0.8"
ff-script           = "0.8"
ff-backend-valkey   = "0.8"   # or ff-backend-postgres
ferriskey           = "0.8"   # direct dep for raw client types
```

```rust
use ff_core::types::FlowId;
use ff_core::contracts::ClaimGrant;
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use ff_scheduler::Scheduler;
```

### After (Valkey, default)

```toml
[dependencies]
flowfabric = "0.8"    # pulls ff-core + ff-sdk + ff-backend-valkey + ff-script
```

```rust
use flowfabric::core::types::FlowId;
use flowfabric::core::contracts::ClaimGrant;
use flowfabric::sdk::{FlowFabricWorker, WorkerConfig};
// or the prelude glob:
use flowfabric::prelude::*;  // re-exports ff_sdk::* (contracts + backend types)
```

### After (Postgres backend)

```toml
[dependencies]
flowfabric = { version = "0.8", default-features = false, features = ["postgres"] }
```

Dropping `default-features` turns off the Valkey backend — `ferriskey`
and `ff-backend-valkey` no longer appear in the transitive graph, and
`flowfabric::postgres` becomes the only backend re-export.

### Opting into scheduler / engine / script internals

Most consumers never touch these directly. If you do (benchmark
harness, custom scanner, alternative worker runtime), opt in per-
crate:

```toml
flowfabric = {
    version = "0.8",
    features = ["valkey", "engine", "scheduler-internals", "script-internals"],
}
```

Reach through the qualified paths: `flowfabric::engine::…`,
`flowfabric::scheduler::…`, `flowfabric::script::…`. These are NOT
flattened into `prelude` — they're rare enough that explicit paths
keep import sites self-documenting.

### Feature matrix

| Feature | Default? | Pulls in |
|---|---|---|
| `valkey`              | yes | `ff-backend-valkey`, `ff-script/valkey-client`, `ff-sdk/valkey-default` |
| `postgres`            | no  | `ff-backend-postgres` |
| `engine`              | no  | `ff-engine` |
| `scheduler-internals` | no  | `ff-scheduler` |
| `script-internals`    | no  | `ff-script` (pure Lua-loader, no ferriskey edge) |
| `iam`                 | no  | propagates `ff-sdk/iam` → `ferriskey/iam` (AWS IAM auth) |

`ff-server` and `ferriskey` are intentionally NOT re-exported by the
umbrella — `ff-server` is a binary/HTTP-server concern (library-mode
consumers almost never need it), and direct `ferriskey` consumption
against FF's public contract is a smell the RFC process prefers to
resolve by expanding `EngineBackend` rather than re-exposing raw
client types. Consumers with a demonstrated need can still pin
`ff-server` or `ferriskey` directly alongside `flowfabric`.

### Version coherence

`flowfabric`'s `Cargo.toml` pins each ff-* sub-crate at the same
workspace version (via `[workspace.package] version`), so
`cargo update -p flowfabric` moves them together. This closes the
class of version-skew bug that hit v0.3.0 and v0.8.0 partial-publishes
at the consumer side.

