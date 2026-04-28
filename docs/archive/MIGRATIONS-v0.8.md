# Archived migration notes — v0.8 (Postgres backend first-class)

> **Archive.** Fell out of the rolling 3-minor window at v0.12.0
> (2026-04-26). Preserved verbatim here for consumers who are
> upgrading across the window boundary. Current window
> (v0.10 / v0.11 / v0.12) lives in `docs/MIGRATIONS.md`.

## v0.8 — Postgres backend first-class + v0.7 cycle rollup

### v0.8.1 — 2026-04-25

Patch release for v0.8.0 publish-infra breakage. No consumer API
impact.

- **Publish order corrected** `[infra]`. `ff-scheduler` now publishes
  before `ff-backend-valkey` (RFC-017 Stage C added
  `ff-backend-valkey → ff-scheduler` dep; publish-list wasn't
  updated). v0.8.0 partial-published ferriskey + ff-core + ff-script
  + ff-observability + ff-observability-http; those were yanked and
  re-released at 0.8.1.
- **Smoke-gate env override** `[infra]`.
  `FF_SMOKE_FANOUT_P99_MS` allows CI hardware variance without hiding
  perf regressions locally (master spec 500 ms stays strict; CI
  runners use 1200 ms).

### v0.8.0 — 2026-04-24

RFC-017 Wave 8 — Postgres backend ships first-class over the full
HTTP surface and the v0.7 `/v1/pending-waitpoints` compatibility
shim is retired.

> Sources preserved verbatim below from the previous
> `docs/CONSUMER_MIGRATION_v0.8.md`. See also
> [`docs/POSTGRES_PARITY_MATRIX.md`](../POSTGRES_PARITY_MATRIX.md) for
> the per-method v0.8.0 parity audit.

#### Breaking

- **`ServerConfig` flat Valkey fields removed** `[break]`. Closes
  v0.7-era deprecation. See [§ ServerConfig reshape](#serverconfig-flat-valkey-fields-removed) below.
- **`PendingWaitpointInfo` wire-shape change** `[break] [wire]`. Raw
  `waitpoint_token` removed; correlate via `(token_kid,
  token_fingerprint)`. `Deprecation: ff-017` header gone. See
  [§ PendingWaitpointInfo](#pendingwaitpointinfo-wire-format-break).
- **`ClaimPolicy::immediate()` removed, `ClaimPolicy::new`
  signature** `[break]`. Now
  `ClaimPolicy::new(worker_id, worker_instance_id, lease_ttl_ms,
  max_wait)`. (v0.7 cycle break, landed at v0.8 tag.)
- **`ReclaimToken::new` gained worker identity args** `[break]`.
  `ReclaimToken::new(grant, worker_id, worker_instance_id,
  lease_ttl_ms)`, mirroring the `ClaimPolicy` shape. (v0.7 cycle.)
- **`EngineBackend` trait expanded by ~17 methods** `[break]` for
  custom backend impls. `HookedBackend` / `PassthroughBackend` /
  `ValkeyBackend` / `PostgresBackend` all carry in-tree impls and are
  the reference for "complete".
- **`ServerError` additive variants** `[break]`. `#[non_exhaustive]`
  variants added across Stages D1–E3 for Postgres-path error
  classification. Catch-all arm required.
- **`ContentionKind::RetryExhausted` additive variant** `[break]`
  (minor — on a `#[non_exhaustive]` enum; blanket `Contention(_)`
  arms keep working).
- **`Server::client()` accessor + `Server::client` field removed**
  `[break]`. Consumers needing Valkey connectivity should build their
  own `ferriskey::ClientBuilder` using the same
  `ServerConfig::valkey` parameters, or go through `EngineBackend`.
- **`Server::fcall_with_reload` removed** `[break]`. Inlined into
  the Valkey backend impl.
- **`Server::scheduler` field removed** `[break]`. Replaced by
  trait-routed dispatch on `backend.claim_for_worker`.

#### Additive

- **Postgres backend first-class** `[additive]`. `FF_BACKEND=postgres`
  boots natively — the v0.7-era `FF_BACKEND_ACCEPT_UNREADY=1` /
  `FF_ENV=development` dev-override is gone;
  `BACKEND_STAGE_READY` now lists both `valkey` and `postgres`.
- **`PostgresScheduler` + 6 reconcilers** `[additive]`. Stage E3
  Postgres execution-dispatch scheduler plus sibling-cancel,
  lease-timeout, completion-listener, cancel-backlog, flow-staging,
  and edge-group-policy reconcilers on the Wave 3 schema.
- **`Server::start_with_backend`** `[additive]`. Test-injection entry
  point accepting a constructed `EngineBackend` trait object
  (`crates/ff-test/tests/http_postgres_smoke.rs` exercises it
  without the env-var dance).
- **`http_postgres_smoke` integration test** `[additive]`. End-to-end
  POST/GET coverage of the HTTP API against `PostgresBackend`, gated
  on the `postgres-e2e` feature.

---

### Upgrading from v0.7 → v0.8 (action list)

1. Bump `ff-*` deps to `0.8.1` (NOT `0.8.0` — partial-publish yanked).
2. Update `ServerConfig` construction sites ([§ below](#serverconfig-flat-valkey-fields-removed)).
3. Drop `waitpoint_token` from your `/v1/pending-waitpoints`
   deserialiser ([§ below](#pendingwaitpointinfo-wire-format-break)).
4. If you match on `ClaimPolicy::immediate()`, adjust to
   `ClaimPolicy::new(..)`.
5. Add a catch-all arm to any `match` on `ServerError`.
6. If migrating to Postgres, set `FF_BACKEND=postgres` +
   `FF_POSTGRES_URL` + `FF_WAITPOINT_HMAC_SECRET`
   ([§ FF_BACKEND=postgres setup](#ff_backendpostgres-setup)).
7. Re-run your integration smoke.

---

### `ServerConfig` flat Valkey fields removed

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

### `PendingWaitpointInfo` wire-format break

`GET /v1/executions/{id}/pending-waitpoints` no longer includes a raw
`waitpoint_token` HMAC in each entry. Clients correlate waitpoints by
`(token_kid, token_fingerprint)`.

Top-level response is the sanitised `PendingWaitpointInfo` array
directly (no `{"entries": [...]}` wrapper). The `Deprecation` header
is gone. Re-sign / verify against `(token_kid, token_fingerprint)`
if you were doing client-side token checks.

The Rust type `ff_core::PendingWaitpointInfo` no longer carries a
`waitpoint_token` field. The Valkey-only inherent
`Server::fetch_waitpoint_token_v07` + `EngineBackend` trait method of
the same name are removed.

### `FF_BACKEND=postgres` setup

Postgres boots natively at v0.8.0 — no `FF_BACKEND_ACCEPT_UNREADY=1` /
`FF_ENV=development` escape hatch.

#### Required env vars

| Env var | Default | Notes |
|---|---|---|
| `FF_BACKEND` | `valkey` | Set to `postgres` to boot the Postgres path. |
| `FF_POSTGRES_URL` | *(empty)* | Required on the Postgres path. `postgres://user:pass@host:port/db` shape (libpq / sqlx). |
| `FF_POSTGRES_POOL_SIZE` | `10` | Max pool connections; ignored on Valkey path. |
| `FF_WAITPOINT_HMAC_SECRET` | *(empty)* | Required on both paths. |

#### Migrations

`ff-server` auto-runs `apply_migrations` at boot against
`FF_POSTGRES_URL`. The initial schema (Waves 3 + 3b) seeds the full
execution / flow / attempt / budget tableset. Re-runs are idempotent.

### `EngineBackend` trait expansion (custom backend impls)

If you implement `EngineBackend` for a bespoke backend (test doubles,
alternate storage), the trait grew by ~17 methods between Wave 4 and
Wave 8. All new methods carry sensible defaults:

- Most return `Err(EngineError::Unavailable { op: "<name>" })`.
- `backend_label` returns `"custom"` if unset.
- `ping` returns `Ok(())` if unset.
- `shutdown_prepare` returns `Ok(())` if unset.

The `HookedBackend`, `PassthroughBackend`, `ValkeyBackend`, and
`PostgresBackend` impls in-tree cover every method concretely and
are the reference for what a "complete" impl looks like.
