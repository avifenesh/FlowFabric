# FlowFabric — Migration Guide (rolling 3-minor window)

Consumer-visible changes for the last three minor versions. **Reverse
chronological** — latest at top. Patch releases nest under their minor.

> **Audience:** library-mode consumers (cairn-fabric, embedders using
> `ff-sdk` / `flowfabric` / `ff-server` / `ff-core` / `ff-engine`
> directly). Pure HTTP-API consumers only need rows labelled
> `[wire]`.
>
> **When this page rolls:** v1.0 release drops v0.7 off the bottom;
> the oldest minor is archived to `docs/archive/MIGRATION_v0.7.md`.
> Per CLAUDE.md §5 item 5, this page is a release-gate artifact —
> every tag PR validates it against `CHANGELOG.md` before tagging.

## Legend

| Tag | Meaning |
|---|---|
| `[break]` | Breaking: existing code stops compiling or changes behaviour without a shim. |
| `[additive]` | New surface; existing code still compiles. Adopt if you want the simplification; ignore otherwise. |
| `[wire]` | HTTP / wire-format change visible to non-Rust consumers. |
| `[infra]` | Environment / deployment / CI change (not a code API change). |

---

## v0.9 — umbrella crate + trait-level boot/seed ergonomics

### v0.9.0 — in prep (tag pending)

v0.9 is **fully additive** on top of v0.8: no consumer source changes
are required to move from v0.8 → v0.9. The release exists to let
consumers *delete* adapter code — one umbrella-crate pin instead of
seven ff-* pins, trait-level HMAC-secret seeding instead of raw HSET,
trait-level boot prep instead of `BackendKind::Valkey` branches.

#### Additive (optional adoption)

- **`flowfabric` umbrella crate** `[additive]`. Re-exports the ff-\*
  family (`ff_core`, `ff_sdk`, and optionally `ff_backend_valkey`,
  `ff_backend_postgres`, `ff_engine`, `ff_scheduler`, `ff_script`)
  behind feature flags. Consumers pin one crate + feature-flag the
  backend instead of tracking 7–8 separate pins in lockstep.
  Default `features = ["valkey"]` preserves the v0.7–v0.8 posture;
  `default-features = false, features = ["postgres"]` opts into the
  Postgres backend. Full before/after + feature matrix in
  [§ umbrella-crate](#umbrella-crate-flowfabric). Closes #279.
- **`ff-sdk` re-exports `ClaimGrant` + `ReclaimGrant` + `ClaimPolicy` +
  `ReclaimToken`** `[additive]`. Consumers typing
  `claim_from_grant` / `claim_from_reclaim_grant` signatures can drop
  the direct `ff-scheduler` pin for type-only imports. `Scheduler`
  itself is intentionally not re-exported — implementing a scheduler
  stays behind the explicit `ff-scheduler` dep. Closes #283.
- **`LeaseSummary` gains `lease_id`, `attempt_index`,
  `last_heartbeat_at`** `[additive]`. Fluent builders
  (`with_lease_id` / `with_attempt_index` /
  `with_last_heartbeat_at`) preserve `#[non_exhaustive]`; construct
  via `LeaseSummary::new(..).with_*(..)`. Valkey backend surfaces all
  three via `describe_execution`, letting consumers delete HGET-based
  lease-summary wrappers. Closes #278.
- **`EngineBackend::seed_waitpoint_hmac_secret`** `[additive]`.
  Trait-level initial HMAC-secret seed. Idempotent per-partition;
  returns `SeedOutcome::{Seeded, AlreadySeeded { same_secret }}`.
  Valkey fans out across partitions with HGET probe + HSET install;
  Postgres single-INSERT against `ff_waitpoint_hmac`. Lets consumers
  drop raw HSET boot paths that blocked PG adoption. Closes #280.
- **`EngineBackend::prepare`** `[additive]`. Trait-level one-time
  boot-prep. Valkey delegates to
  `ff_script::loader::ensure_library` (FUNCTION LOAD + retry);
  Postgres returns `NoOp` (migrations run out-of-band). Idempotent,
  safe on every boot. Lets consumers drop
  `if let BackendKind::Valkey = ...` boot branches. Closes #281.

#### Fixed (no consumer action — listed for awareness)

- **Cluster-topology bootstrap race** `[infra]`. On a just-formed
  6-node Valkey cluster, `FUNCTION LOAD` could route to a node whose
  topology view still classified it as primary though it had
  transitioned to replica in gossip — surfaced as `READONLY` during
  `Server::start_with_metrics`. Two-part fix:
  `.github/cluster/bootstrap.sh` now polls `cluster_state:ok` + 3
  masters / 3 slaves on every node before declaring ready (#276); and
  `ff_script::loader::ensure_library` retry window extended
  3×1s = 4s → 6 attempts @ 1/2/4/4/4s = 15s (#289). Closes #275.
  Follow-up ferriskey robustness tracked at #290.

---

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
> [`docs/POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md) for
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

#### v0.7-cycle content (shipped inside 0.8.0, no v0.7 tag)

See [§ v0.7 cycle content](#v07--content-shipped-inside-v080) below
for the full list (caps extraction, handle codec with `BackendTag`,
`Handle.opaque` wire-v2, `rotate_waitpoint_hmac_secret_all`
trait method, Valkey `claim` + `claim_from_reclaim` implementations,
Postgres flow family, `ClaimPolicy` / `ReclaimToken` reshape).

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
7. Re-run your integration smoke. File issues against
   [`rfcs/RFC-017-ff-server-backend-abstraction.md`](../rfcs/RFC-017-ff-server-backend-abstraction.md)
   for any migration-guide gap you hit.

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

`ValkeyServerConfig` is **not** `#[non_exhaustive]` — you can
construct the struct literal directly. Adding a field is a v0.y.0
breaking bump; insulate with `..ValkeyServerConfig::default()` if you
prefer.

### `PendingWaitpointInfo` wire-format break

`GET /v1/executions/{id}/pending-waitpoints` no longer includes a raw
`waitpoint_token` HMAC in each entry. Clients correlate waitpoints by
`(token_kid, token_fingerprint)`.

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

#### Parity

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

### Umbrella crate (`flowfabric`)

Added in v0.9.0 per issue
[#279](https://github.com/avifenesh/FlowFabric/issues/279). Consumers
pinning the 7-crate ff-* family in lockstep can switch to a single
`flowfabric` pin and let Cargo resolve a coherent set.

> Note: this lands in the **v0.9** release, but the before/after
> shape reads most naturally alongside the rest of the v0.8 migration
> content since the "before" is the v0.8 multi-pin world. Kept here
> for that reason; the v0.9 section above carries the headline entry.

#### Before (v0.8)

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

#### After (v0.9 — Valkey, default)

```toml
[dependencies]
flowfabric = "0.9"    # pulls ff-core + ff-sdk + ff-backend-valkey + ff-script
```

```rust
use flowfabric::core::types::FlowId;
use flowfabric::core::contracts::ClaimGrant;
use flowfabric::sdk::{FlowFabricWorker, WorkerConfig};
// or the prelude glob:
use flowfabric::prelude::*;  // re-exports ff_sdk::* (contracts + backend types)
```

#### After (v0.9 — Postgres backend)

```toml
[dependencies]
flowfabric = { version = "0.9", default-features = false, features = ["postgres"] }
```

Dropping `default-features` turns off the Valkey backend — `ferriskey`
and `ff-backend-valkey` no longer appear in the transitive graph, and
`flowfabric::postgres` becomes the only backend re-export.

#### Opting into scheduler / engine / script internals

Most consumers never touch these directly. If you do (benchmark
harness, custom scanner, alternative worker runtime), opt in per-
crate:

```toml
flowfabric = {
    version = "0.9",
    features = ["valkey", "engine", "scheduler-internals", "script-internals"],
}
```

Reach through qualified paths: `flowfabric::engine::…`,
`flowfabric::scheduler::…`, `flowfabric::script::…`. These are NOT
flattened into `prelude` — they're rare enough that explicit paths
keep import sites self-documenting.

#### Feature matrix

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

#### Version coherence

`flowfabric`'s `Cargo.toml` pins each ff-* sub-crate at the same
workspace version (via `[workspace.package] version`), so
`cargo update -p flowfabric` moves them together. This closes the
class of version-skew bug that hit v0.3.0 and v0.8.0 partial-publishes
at the consumer side.

---

## v0.7 — content shipped inside v0.8.0

No `v0.7.x` tag was ever published. Changes from the v0.7 RFC cycle
ship verbatim inside the v0.8.0 release; they're itemised here for
consumers moving from a pre-v0.8.0 pin (or for readers reconstructing
when each change landed).

### Added

- **RFC-v0.7 Wave 2: Valkey `EngineBackend::claim` +
  `claim_from_reclaim` real impls** `[additive]`. `ValkeyBackend::claim`
  performs a fresh-find scan across execution partitions
  (ZRANGEBYSCORE on the lane's eligible set, capability-subset filter
  via `ff_core::caps::matches`, `ff_issue_claim_grant`, and claim via
  `ff_claim_execution`) and returns `HandleKind::Fresh`.
  `claim_from_reclaim` unwraps the `ReclaimGrant` carried by
  `ReclaimToken` and routes through `ff_claim_resumed_execution`,
  returning `HandleKind::Resumed` with a bumped lease epoch. Replaces
  prior `EngineError::Unavailable` stubs.
- **RFC-v0.7 Wave 4c: Postgres flow family** `[additive]`.
  `PostgresBackend` implements six flow-scoped trait methods over the
  Wave 3 schema: `describe_flow`, `list_flows`, `list_edges`,
  `describe_edge`, `cancel_flow`, `set_edge_group_policy`.
  `cancel_flow` runs a SERIALIZABLE cascade with a 3-attempt retry
  loop (Q11); exhaustion surfaces as
  `EngineError::Contention(ContentionKind::RetryExhausted)` so
  consumers fall back to the cancel-backlog reconciler.
- **`ContentionKind::RetryExhausted` variant** `[additive]`.
  `#[non_exhaustive]` enum; classified `Retryable` via the blanket
  `Contention(_)` arm.
- **`EngineBackend::rotate_waitpoint_hmac_secret_all`** `[additive]`
  (RFC-v0.7 Wave 1b, migration-master Q4). Cluster-wide waitpoint
  HMAC kid rotation. Valkey fan-out one
  `ff_rotate_waitpoint_hmac_secret` FCALL per execution partition;
  Postgres Wave 4 resolves to a single INSERT into
  `ff_waitpoint_hmac(kid, secret, rotated_at)`. Wave 1b ships the
  Postgres stub. SDK exposes
  `FlowFabricWorker::rotate_waitpoint_hmac_secret_all`. The
  pre-existing per-partition free-fn
  `ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions` is
  unchanged for backwards compat.
- **`ff_core::waitpoint_hmac` re-export module** `[additive]`.
  Consumer-facing import path for `WaitpointHmac`, `VerifyingKid`,
  `WaitpointHmacKids` + rotation args/outcomes. Signing and
  verification remain server-side (Lua on Valkey; Wave 4 stored procs
  on Postgres); the module doc captures the sign/verify-location
  contract.

### Changed

- **Pre-1.0 BREAKING — `ClaimPolicy` extended with worker identity**
  `[break]`. Now
  `ClaimPolicy::new(worker_id, worker_instance_id, lease_ttl_ms,
  max_wait)` — the prior `ClaimPolicy::immediate()` and
  `ClaimPolicy::with_max_wait(..)` constructors are replaced. The
  backend's `claim(&self, lane, caps, policy)` can now invoke
  `ff_claim_execution` without a side-channel identity lookup.
- **Pre-1.0 BREAKING — `ReclaimToken` extended with worker identity**
  `[break]`. Mirrors the `ClaimPolicy` shape. New constructor:
  `ReclaimToken::new(grant, worker_id, worker_instance_id,
  lease_ttl_ms)`.
- **RFC-v0.7 Wave 1a: `ff-core::caps` extracted** `[additive]`. The
  capability subset predicate previously lived as
  `ff_scheduler::claim::caps_subset` (private) and was implicitly
  duplicated by `lua/scheduling.lua`. New public surface:
  `ff_core::caps::{matches, matches_csv, CapabilityRequirement}`.
  `matches_csv` is a line-for-line replacement of the old private
  helper (same case-sensitive subset semantics, same
  empty-required-matches-any default). No behaviour change. Ref
  RFC-v0.7 §Q7, PR #230.
- **RFC-v0.7 Wave 1c: `Handle.opaque` codec moved to
  `ff_core::handle_codec` with embedded `BackendTag`** `[additive]`.
  Byte-layout / wire-version logic that lived at
  `ff_backend_valkey::handle_codec` is now a public `ff_core` module
  so the future Postgres backend decodes the same shape. New wire
  format v2 prefixes `0x02 magic + 1-byte wire version + 1-byte
  BackendTag`; pre-Wave-1c (v1) Valkey handles still decode via a
  compat path keyed off legacy leading byte `0x01`. New public
  types: `ff_core::handle_codec::{HandlePayload, DecodedHandle,
  encode, decode, HandleDecodeError}`; new enum variant
  `BackendTag::Postgres`; new enum variant
  `ValidationKind::HandleFromOtherBackend` (returned by backends
  when a handle tagged for one backend is presented to another). No
  behaviour change for existing Valkey call sites.

### Fixed

- **`ff-core` unit-test compile fix** `[infra]`.
  `backend_config_valkey_ctor` used an irrefutable `let` pattern that
  broke when the `Postgres` variant of `BackendConnection` landed;
  switched to `let-else` with a descriptive panic. Pure test-only.

---

## Retired (pre-v0.7)

Archived migration notes at `docs/archive/MIGRATION_v0.6.md` and
earlier (when/if created — no per-version migration docs existed
before v0.8; consult `CHANGELOG.md` entries for v0.6/v0.5/v0.4/v0.3
directly). Current window: v0.7 / v0.8 / v0.9.
