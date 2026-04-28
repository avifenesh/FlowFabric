# FlowFabric — Migration Guide (rolling 3-minor window)

Consumer-visible changes for the last three minor versions. **Reverse
chronological** — latest at top. Patch releases nest under their minor.

> **Audience:** library-mode consumers (cairn-fabric, embedders using
> `ff-sdk` / `flowfabric` / `ff-server` / `ff-core` / `ff-engine`
> directly). Pure HTTP-API consumers only need rows labelled
> `[wire]`.
>
> **When this page rolls:** each minor release drops the oldest in
> the 3-minor window off the bottom; the archived minor moves to
> `docs/archive/MIGRATIONS-v<minor>.md`. Current rolling window:
> **v0.10 / v0.11 / v0.12**. v0.8 was archived at v0.12.0 (see
> [`docs/archive/MIGRATIONS-v0.8.md`](archive/MIGRATIONS-v0.8.md)).
> v0.9 content is preserved in-page for now because the umbrella-
> crate appendix it relies on is referenced across the window.
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

## v0.12 — SQLite dev-only backend (RFC-023)

### v0.12.0 — 2026-04-26

Third `EngineBackend` implementation lands: **SQLite**, scoped
permanently to dev-only / `cargo test` / contributor onboarding per
[RFC-023](../rfcs/RFC-023-sqlite-dev-only-backend.md). Mostly additive;
two minor breaking shape adjustments on ff-server types fall out of
wiring the third backend.

Positioning: **SQLite is a testing harness; Valkey is the engine;
Postgres is the enterprise persistence layer.** SQLite is NOT a
deployment target — see [`DEPLOYMENT.md`](DEPLOYMENT.md) §0 and
[`dev-harness.md`](dev-harness.md).

See `docs/CONSUMER_MIGRATION_0.12.md` for the full consumer
checklist; this page gives the rolling-window summary.

#### Breaking

- **`ServerError` gains `SqliteRequiresDevMode` variant + becomes
  `#[non_exhaustive]`** `[break]`. Consumers writing exhaustive
  `match` arms on `ServerError` must add a wildcard arm (or the new
  variant). Pre-v0.12 the enum was not sealed; sealing it now means
  future variant additions are additive.
- **`ServerConfig` gains `sqlite: SqliteServerConfig` field + becomes
  `#[non_exhaustive]`** `[break]`. Consumers constructing
  `ServerConfig` via struct literal must migrate to
  `ServerConfig::from_env()`, the new `ServerConfig::sqlite_dev()`
  builder, or `..Default::default()` spread.
- **`ff_sdk::worker` module cfg-gating shift** `[break]` (forward-
  looking; no shipped consumer exercises this today). The
  `FlowFabricWorker` type + `connect_with` re-export move from
  `valkey-default`-gated to always-compiled. The Valkey-specific
  methods (`connect`, `claim_next`, `claim_from_grant`,
  `claim_via_server`, `claim_from_reclaim_grant`,
  `claim_resumed_execution`, `claim_execution`,
  `read_execution_context`, `deliver_signal`) stay at the item level
  and are ABSENT under `--no-default-features`. Consumers on non-
  Valkey backends drive ops through `worker.backend()` and the
  `EngineBackend` trait directly.

#### Additive

- **`ff-backend-sqlite` crate published** `[additive]`. Third
  `EngineBackend` impl; `cargo add ff-backend-sqlite` (or enable
  `flowfabric`'s future `sqlite` feature). See
  [`dev-harness.md`](dev-harness.md) for the canonical setup.
- **`BackendKind::Sqlite`, `SqliteServerConfig`,
  `ServerConfig::sqlite_dev()`, `SqliteBackend::new`,
  `BackendError::RequiresDevMode`, `BackendTag::Sqlite` (wire byte
  `0x03`)** `[additive]`. New public APIs per RFC-023 §4.4.
  `BackendTag` was already `#[non_exhaustive]`; every consumer match
  site uses wildcard or inequality checks, so the `Sqlite` variant
  addition is compile-safe.
- **`ff_sdk::SqliteBackend` re-export** `[additive]`. Under
  `ff-sdk = { default-features = false, features = ["sqlite"] }`,
  consumers can name `ff_sdk::SqliteBackend` without pinning
  `ff-backend-sqlite` directly.
- **Production guard — `FF_DEV_MODE=1` required** `[additive]`.
  `FF_BACKEND=sqlite` and `SqliteBackend::new` both refuse to
  activate without `FF_DEV_MODE=1`; guard lives on the type so
  every path pays it. Banner emits at WARN level on every
  construction. Orthogonal to `FF_ENV=development` /
  `FF_BACKEND_ACCEPT_UNREADY=1`.
- **Scanner supervisor collapses to N=1 on SQLite** `[additive]`.
  Single-writer / single-process → no partition fan-out. Ports
  `budget_reset` reconciler; other reconcilers land in subsequent
  phases per RFC-023 §4.3 parity commitment.

#### Infra

- **New env vars** `[infra]`. Added to the `ServerConfig::from_env`
  surface:
  - `FF_BACKEND=sqlite` — selects the SQLite backend (alongside
    `valkey` / `postgres`).
  - `FF_DEV_MODE=1` — SQLite-specific production guard. Required
    for `FF_BACKEND=sqlite` and direct `SqliteBackend::new`
    construction. Has no effect on other backends.
  - `FF_SQLITE_PATH` (default `:memory:`) — file path or URI.
    Supports `:memory:?cache=shared`,
    `file:name?mode=memory&cache=shared` (per-test isolation),
    `/tmp/ff-dev.db` (file-backed).
  - `FF_SQLITE_POOL_SIZE` (default `4`) — 1 writer + N–1 readers.
- **SQLite migrations 0001–0014** `[infra]`. Hand-ported SQLite-
  dialect migrations in `crates/ff-backend-sqlite/migrations/`,
  1:1 numbered with Postgres for parity-drift detection. Applied
  idempotently on pool init via `sqlx::migrate!`; first-time SQLite
  users need nothing extra.
- **Postgres migration 0016 — `ff_exec_core.started_at_ms`**
  `[infra]`. Additive, forward-only. Set-once first-claim
  timestamp; drops the LATERAL / correlated-subquery on
  `ff_attempt.started_at_ms` from the Wave-9 Spine-B read path.
  Backfilled from `MIN(ff_attempt.started_at_ms)` per execution.
  Migration number 0015 is reserved for RFC-024 (claim-grant
  table) — renumbers safely if RFC-024 lands first.

#### Parity matrix

See [`POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md) (which
gains a SQLite column at v0.12). Every Wave-9 method `impl` on PG
at v0.11 is `impl` on SQLite at v0.12, with the documented
exceptions: `claim_for_worker` is `stub` on SQLite (no scheduler —
RFC-023 §5 non-goal); `subscribe_instance_tags` is `n/a` on all
three backends per #311.

---

## v0.11 — Postgres Wave 9 (12 capability flips + 5 migrations)

### v0.11.0 — 2026-04-26

Full parity on the Postgres backend for the remaining Wave-9 surface
(RFC-020 Rev 7). 12 `PostgresBackend::capabilities().supports` flags
flip `false → true`:

- Operator control: `cancel_execution`, `change_priority`,
  `replay_execution`, `revoke_lease`
- Read model: `read_execution_info`, `read_execution_state`,
  `get_execution_result`
- Budget / quota admin: `budget_admin`, `quota_admin`
- Waitpoints: `list_pending_waitpoints`
- Flow cancel split: `cancel_flow_header`, `ack_cancel_member`

No Rust API change. No wire-format change. Valkey backend behaviour
unchanged per RFC-020 §5.2.

See `docs/CONSUMER_MIGRATION_0.11.md` for the full consumer
checklist + migration-ordering notes; this page gives the
rolling-window summary.

#### Infra

- **Postgres migrations 0010–0014** `[infra]`. All additive, forward-
  only. Must be applied in order **before** serving v0.11.0 traffic
  on a Postgres deployment. `ff-server` auto-runs them at boot via
  `apply_migrations`; operators managing schema out-of-band must
  apply them manually (e.g. `sqlx migrate run`).
  - `0010_operator_event_outbox.sql` — `ff_operator_event` + trigger
    + `NOTIFY ff_operator_event` channel.
  - `0011_waitpoint_pending_extensions.sql` — adds `state`,
    `required_signal_names`, `activated_at_ms` columns on
    `ff_waitpoint_pending`.
  - `0012_quota_policy.sql` — `ff_quota_policy` + `ff_quota_window`
    + `ff_quota_admitted` tables (256-way HASH-partitioned).
  - `0013_budget_policy_extensions.sql` — scheduling + breach +
    definitional columns on `ff_budget_policy`.
  - `0014_cancel_backlog.sql` — `ff_cancel_backlog` table driving
    `cancel_flow_header` + `ack_cancel_member`.

#### Additive

- **12 Postgres `Unavailable` methods now serve real responses**
  `[additive]`. Consumers grey-rendering operator UI actions based
  on `capabilities().supports.<field>` will see the flags flip to
  `true` after upgrading + running migrations. No consumer code
  change required; audit UI code for stale "Postgres does not
  support X" copy.

---

## v0.10 — read-side ergonomics (capabilities + typed subscribe + filter)

### v0.10.0 — 2026-04-26

Consumer-facing read-side surface lands in one minor: flat capability
struct (#277), typed stream-event enums (#282 Stage C), and a required
`ScannerFilter` parameter on three `subscribe_*` trait methods
(#282). Two additive: `EngineBackend::suspend_by_triple` (#322) and
`ff_core::handle_codec::v1_handle_for_tests` (#323, behind
`test-fixtures` feature).

See `docs/CONSUMER_MIGRATION_0.10.md` for the full before/after
snippets + upgrade checklist; this page gives the rolling-window
summary.

Only consumers that directly read `EngineBackend::capabilities_matrix()`
(or re-exported `CapabilityMatrix` / `Capability` / `CapabilityStatus`
via `flowfabric::core::capability`) or call the `subscribe_*` trait
methods are affected.

#### Breaking

- **`EngineBackend::capabilities_matrix()` → `capabilities()`**
  `[breaking]`. Return type `CapabilityMatrix` → `Capabilities`.
  `BTreeMap<Capability, CapabilityStatus>` replaced by flat `Supports`
  struct with named bool fields.

  ```rust
  // v0.9.x:
  use flowfabric::core::capability::{Capability, CapabilityMatrix, CapabilityStatus};
  let caps: CapabilityMatrix = backend.capabilities_matrix();
  if matches!(caps.get(Capability::CancelExecution), CapabilityStatus::Supported) {
      // ...
  }

  // v0.10+:
  use flowfabric::core::capability::{Capabilities, Supports};
  let caps: Capabilities = backend.capabilities();
  if caps.supports.cancel_execution {
      // ...
  }
  ```

  Partial-status nuance (e.g. non-durable cursor on Valkey
  `subscribe_completion`) moved to rustdoc on the trait method and
  `docs/POSTGRES_PARITY_MATRIX.md`; the flat bool answers "is this
  callable at all," and the per-backend semantic lives in docs.

  Group-level fields (`budget_admin`, `quota_admin`) roll up sibling
  methods per cairn's grouping preference — one bool for the operator
  UI grey-render, fine-grained pre-dispatch checks still use
  `EngineError::Unavailable`. See
  [`Supports`](../crates/ff-core/src/capability.rs) rustdoc for the
  per-field mapping.

---

## v0.9 — umbrella crate + trait-level boot/seed ergonomics

### v0.9.0 — shipped 2026-04-25

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

## v0.8 — Postgres backend first-class + v0.7 cycle rollup (archived)

> **Archived at v0.12.0.** The full v0.8 release body (breaking
> changes, upgrading checklist, `ServerConfig` reshape,
> `PendingWaitpointInfo` wire-format break, `FF_BACKEND=postgres`
> setup, `EngineBackend` trait expansion) now lives at
> [`docs/archive/MIGRATIONS-v0.8.md`](archive/MIGRATIONS-v0.8.md).
> The umbrella-crate appendix referenced from the v0.9 entry is
> preserved in-page below because it is cross-referenced inside the
> current rolling window.


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


## Retired (pre-v0.9)

Current rolling window: **v0.10 / v0.11 / v0.12**. v0.8 was archived
at v0.12.0 to
[`docs/archive/MIGRATIONS-v0.8.md`](archive/MIGRATIONS-v0.8.md); the
umbrella-crate appendix referenced from the v0.9 entry stays in-page
above because it is cross-referenced inside the window. Fully-retired
content older than v0.8 is archived at
https://github.com/avifenesh/flowfabric-archive/blob/main/docs/MIGRATIONS-pre-v0.8.md
(private). For older versions, consult `CHANGELOG.md` entries for
v0.6 / v0.5 / v0.4 / v0.3 directly.
