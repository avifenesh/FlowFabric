# RFC-022: SQLite — single-node local-mode backend

**Status:** DRAFT
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Target release:** TBD — open question (see §9.4)
**Related RFCs:** RFC-010 §9.2 (archived — first mention of SQLite as a future backend option), RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction + staged Postgres parity), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions), RFC-020 (Postgres Wave 9)
**Tracking issue:** #338
**Related artifacts:**
- `crates/ff-backend-postgres/src/lib.rs` — durable-backend reference impl (1122 LOC)
- `crates/ff-backend-postgres/migrations/` — nine SQL migrations baseline
- `crates/ff-core/src/engine_backend.rs` — ~90 trait methods
- `crates/ff-backend-valkey/src/lib.rs` — alternate backend shape
- `crates/ff-server/src/config.rs` — `FF_BACKEND` resolution today

> **Draft status:** this RFC proposes a third backend. It is NOT accepted.
> Schema strategy, parity scope, and release target are subject to review.
> Section 9 (Open questions) lists the genuine forks the owner must
> adjudicate before acceptance.

---

## 1. Summary

FlowFabric today ships two backends: Valkey (distributed, in-memory, RFC-012
era) and Postgres (durable, multi-node, RFC-017 Stage E4 first-class in v0.8).
Both require an external service at development and test time: a Valkey
instance (~100 MB idle) or a Postgres cluster (Docker, migrations, user
setup). This RFC proposes a third backend — **SQLite** — targeting exactly
the single-node, no-external-service use cases the first two address
poorly: `cargo test` without Docker, single-binary embedded deployments
of ff-server as an in-process library, and consumer integration tests
that want the whole FF stack in one process. SQLite is **not** a
replacement for Postgres or Valkey in any production deployment recommendation;
it is a local-mode backend whose scope is bounded by SQLite's single-writer
and single-process realities.

---

## 2. Motivation

### 2.1 Concrete use cases

1. **`cargo test` without Docker.** Today every ff-server integration
   test that wants to exercise real backend behaviour spins up either a
   Valkey container or a Postgres container; CI minutes and contributor
   friction both scale with this. A file-backed or `:memory:` SQLite
   backend removes the external-service requirement for the subset of
   tests that don't need distributed semantics.
2. **Single-binary embedded deployments.** Consumers embedding ff-server
   as an in-process library (i.e. linking `ff-server` into their own
   binary rather than running it as a sidecar) need a backend that ships
   in-process. Neither Valkey (requires a network service) nor Postgres
   (same) qualifies. SQLite is file-or-memory-backed and lives entirely
   in the host process.
3. **CI tests that don't want a shared Postgres.** Consumer projects
   running FF against a shared Postgres across test matrices hit
   cross-test contamination (migrations, leftover rows, unique-ID
   collisions across parallel test runners). A per-test
   `FF_SQLITE_PATH=:memory:` sidesteps the shared-state category entirely.
4. **Consumer integration tests, whole FF stack in one process.** The
   cairn-fabric pattern — consumer code + ff-server + backend in one
   `cargo test` binary — currently requires an ambient Valkey. SQLite
   makes that pattern zero-setup for the subset of consumer tests that
   don't exercise distributed or high-throughput paths.

### 2.2 Why SQLite specifically

Of the candidate in-process or file-backed stores (SQLite, DuckDB,
sled, redb, an in-memory mock), SQLite is the only one that:

- Exercises real transactional SQL behaviour (the Postgres backend's
  schema is SQL-shaped; a SQL-shaped test harness is higher-fidelity
  than a hand-rolled mock).
- Is already in the workspace dependency tree transitively via `sqlx`
  (Cargo.toml workspace root pins `sqlx = "0.8"` default-features-off;
  enabling the `sqlite` feature is a one-line cost).
- Ships a `:memory:` mode that is effectively free for per-test setup.

A mock backend in-tree was considered and rejected in §7.

---

## 3. Surface impact on consumers

### 3.1 Configuration

- `FF_BACKEND=sqlite` — new value alongside `valkey` (default) and
  `postgres` (RFC-017 Stage E4 first-class).
- `FF_SQLITE_PATH=<path>` — file path for file-backed mode, or the
  literal string `:memory:` for in-memory (test) mode. No default;
  required when `FF_BACKEND=sqlite`.
- Optional `FF_SQLITE_POOL_SIZE` — read-pool size (SQLite's single-writer
  constraint keeps the write connection at 1 regardless; see §5.4).
- Optional `FF_SQLITE_WAL=true|false` — WAL mode toggle (default open;
  see §9.2).

`ServerConfig::BackendKind` grows a third variant `Sqlite`; the
`sqlite` field mirrors the shape of `postgres` and `valkey`
sub-configs already on `ServerConfig`.

### 3.2 Trait surface

**Zero new trait methods.** The SQLite backend implements the existing
`EngineBackend` surface (~90 methods per RFC-012). Capability flags
(RFC-018) report the parity shape per §6.

### 3.3 Capability matrix

Consumers read capabilities via RFC-018's discovery surface; a consumer
that correctly consumes capabilities (cairn-fabric does) greys out
SQLite-unsupported surfaces automatically. No consumer code change is
required to adopt `FF_BACKEND=sqlite` beyond setting the env var.

---

## 4. Scope boundaries

### 4.1 What SQLite IS

- A **single-process** backend. A single ff-server instance (or a
  single test binary) holds the SQLite handle. Cross-process coordination
  is out of scope for v1.
- A **file-backed or `:memory:`** backend. File mode durable across
  process restarts; `:memory:` for tests.
- A **dev / test / embedded** backend. Appropriate for local
  development, CI, single-node demos, and in-process consumer embedding.

### 4.2 What SQLite IS NOT

- **Not multi-process safe under contention.** SQLite's file-locking
  degrades sharply with concurrent writers, even with WAL. Running
  two ff-server processes against the same SQLite file is unsupported
  in v1 and will NOT be made to work before acceptance.
- **Not clustered.** rqlite, litestream-for-HA, and Litestream-style
  streaming replicas are explicit non-goals for v1 (see §8).
- **Not high-throughput.** Expected throughput is roughly 10× lower
  than single-node Postgres on the hot path — acceptable for dev and
  test; unsuitable for production workloads that care about latency
  or QPS.

### 4.3 Expected performance envelope

Rough order-of-magnitude only (no benchmark data yet; see §9.3):

| Dimension         | Valkey      | Postgres (1-node) | SQLite (WAL, file) |
|-------------------|-------------|-------------------|--------------------|
| Create + dispatch | ~sub-ms     | ~ms               | ~ms                |
| Claim + complete  | ~sub-ms     | ~ms               | ~ms                |
| Subscribe latency | ~sub-ms     | ~ms (NOTIFY)      | ~sub-ms (in-proc)  |
| Write QPS ceiling | 10^5+       | 10^4              | 10^3               |
| Multi-process?    | Yes         | Yes               | **No**             |

SQLite's subscribe latency is expected to beat Postgres because the
pub/sub fan-out is in-process (tokio broadcast, §5.2) rather than a
LISTEN/NOTIFY round-trip — but only within a single process.

---

## 5. Design sketch

### 5.1 Crate layout

New crate: `crates/ff-backend-sqlite/`.
Mirrors `crates/ff-backend-postgres/` shape:
- `src/lib.rs` — `SqliteBackend` struct implementing `EngineBackend`
- `src/*` — per-operation modules, split along the same seams as the
  Postgres backend (hot-path / subscriptions / admin / scanners)
- `migrations/` — SQLite-dialect migrations (see §5.3)
- `tests/capabilities.rs` — parity assertions per §6

Workspace additions:
- `sqlx` features gain `"sqlite"` and `"runtime-tokio"` (already present
  for Postgres).
- `ff-server` grows a `sqlite` Cargo feature mirroring `postgres`.
- `ff-server::config` grows `BackendKind::Sqlite` + `SqliteServerConfig`.

### 5.2 Pub/sub replacement for LISTEN/NOTIFY

The Postgres backend wires subscriptions through `pg_notify` triggers
(migrations 0001, 0006, 0007; `fn_notify_*` functions) — a feature
SQLite does not have. Two replacement options:

**Option A (PROPOSED): in-process `tokio::sync::broadcast` channels.**
Every subscription type (`subscribe_lease_history`,
`subscribe_completion`, `subscribe_signal_delivery`, stream frames)
gets a process-global `broadcast::Sender` on `SqliteBackend`. Each
write path that would emit a `pg_notify` in the Postgres backend
instead sends on the broadcast channel inside the same transaction
boundary (emit on commit; never emit on rollback). Subscribers hold
`broadcast::Receiver` and fan out to the existing `Stream` surface
from RFC-019.

Tradeoffs: **zero cross-process fan-out** (fine — single-process is
the scope per §4). Lower latency than NOTIFY. Straightforward to
implement. Committed-before-emit ordering is preserved because the
emit site runs after `tx.commit()` completes.

**Option B (fallback, not for v1): tight-loop polling.** A 100 ms
tick polls the outbox tables for new `event_id` rows. Latency
floor 100 ms; CPU cost constant even when idle. Supports cross-process
if v2 ever needs it. Documented here as the fallback if a real
cross-process-dev use case surfaces post-v1.

### 5.3 Schema strategy — DECISION: separate SQLite migrations

The Postgres migrations cannot be shared or symlinked. Survey of
`crates/ff-backend-postgres/migrations/*.sql` shows fundamental
dialect incompatibilities:

| Postgres feature used                        | SQLite support       |
|----------------------------------------------|----------------------|
| `PARTITION BY HASH (partition_key)` with 256 children | **None** |
| `DO $$ BEGIN FOR ... LOOP ... END $$`        | **None**             |
| `BIGSERIAL` / `GENERATED ALWAYS AS IDENTITY` | Use `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `jsonb` type + `'{}'::jsonb` cast            | Store as `TEXT`; JSON1 extension for ops |
| `BYTEA`                                      | `BLOB`                |
| `CREATE TRIGGER ... PERFORM pg_notify(...)`  | **None** — replaced by §5.2 Option A |
| `FOR UPDATE` row locks                       | Serializable txns (different mechanism, same effect) |
| `ON CONFLICT ... DO UPDATE`                  | Supported (3.24+)     |
| `RETURNING`                                  | Supported (3.35+) — see §9.1 |

Partitioning is the load-bearing incompatibility: every major FF
table (`ff_exec_core`, `ff_flow_core`, `ff_attempt`, the
outbox, the budget tables) is `PARTITION BY HASH (partition_key)` with
256 children in the Postgres baseline. SQLite has no table-partitioning
primitive. **The SQLite schema drops partitioning entirely** — a single
non-partitioned table per entity. This is acceptable under the §4
single-process / low-throughput envelope; it is not acceptable for
Postgres, which is why Postgres partitions.

**Consequence:** `crates/ff-backend-sqlite/migrations/` is a
hand-ported set of SQLite-dialect files, authored once and maintained
alongside Postgres migrations. The maintenance cost is: every Postgres
migration that changes schema (not just data) gets a matching SQLite
migration in the same PR. The `ff-backend-valkey` crate has no
SQL schema at all, so the parallel-maintenance burden lands on SQLite
alone.

A shared-migrations approach (symlinks, a macro, a templating layer)
was considered and rejected — the dialect gap is wide enough that
sharing would force the lowest-common-denominator on the Postgres
schema, which would regress Postgres throughput for a dev/test backend.

### 5.4 Connection pool

`sqlx::SqlitePool` with two logical pools:

- **1 writer connection** — SQLite permits only one concurrent writer;
  attempts to write from multiple connections serialize at the file
  lock. We make this explicit by holding exactly one writer in the pool.
- **N reader connections** — WAL mode permits concurrent readers
  alongside the writer. Default N = 4; configurable via
  `FF_SQLITE_POOL_SIZE`.

Every write path acquires the writer; every read path acquires a reader.
`:memory:` mode uses `file::memory:?cache=shared` so connections in
the same pool see the same database.

### 5.5 Transaction isolation

SQLite is serializable by default; WAL mode preserves serializability
with improved concurrency. Every transaction that the Postgres backend
runs `SERIALIZABLE` (suspend, signal, outbox emit) runs with default
SQLite isolation here — no additional configuration required. No
advisory-lock equivalent is needed because SQLite's single-writer
model makes serialization implicit.

### 5.6 `:memory:` mode specifics

- Schema migrations run on every pool connect (no persistent state
  across process restarts by definition).
- Suitable for `cargo test` per-test fixtures.
- `file::memory:?cache=shared` variant for multi-connection tests that
  need the writer + readers to share state.

---

## 6. Parity matrix (v1 candidate)

Proposed v1 parity = "Postgres v0.10 parity **minus** Wave 9 deferrals
(per RFC-020)". Identical shape keeps the three-backend mental model
consistent.

| Group                              | Postgres v0.10 | SQLite v1    |
|------------------------------------|---------------|--------------|
| Create / claim / complete / fail   | YES           | **YES**      |
| Subscribe_* (RFC-019)              | YES           | **YES** (§5.2) |
| Suspend / signal / waitpoint       | YES           | **YES**      |
| Timeout scanners                   | YES           | **YES**      |
| Budget / quota hot path            | YES           | **YES**      |
| Lease / signal event outbox        | YES           | **YES**      |
| RFC-020 Group 1 — flow cancel split        | DEFER | **DEFER** |
| RFC-020 Group 2 — read_execution_*  model  | DEFER | **DEFER** |
| RFC-020 Group 3 — operator control         | DEFER | **DEFER** |
| RFC-020 Group 4 — budget/quota admin       | DEFER | **DEFER** |
| RFC-020 Group 5 — list_pending_waitpoints  | DEFER | **DEFER** |
| RFC-020 Group 6 — subscribe_instance_tags  | DEFER | **DEFER** |

The Wave 9 deferrals on Postgres return
`EngineError::Unavailable { op }`; SQLite v1 returns the same for the
same methods. When Postgres Wave 9 lands (per RFC-020), SQLite lands
the equivalent impl in a follow-up. Keeping the deferral set aligned
means a consumer doesn't need a per-backend exception list.

All ~7 current `EngineError::Unavailable` sites in
`crates/ff-backend-postgres/src/lib.rs` are mirrored as SQLite v1
stubs.

---

## 7. Non-goals (explicit)

1. **Not distributed.** No rqlite, no cluster mode, no leader election.
   A consumer wanting distributed semantics picks Valkey or Postgres.
2. **Not a production high-throughput backend.** The §4.3 envelope is
   dev/test; shipping SQLite as a production recommendation is
   out of scope.
3. **Not cross-process safe under contention.** Running multiple
   ff-server processes against one SQLite file is unsupported and
   will not be tested. The §5.2 Option A broadcast pub/sub is
   per-process by construction.
4. **Not a replacement for Valkey or Postgres.** This RFC does not
   propose changing the default (`FF_BACKEND=valkey`), does not
   propose deprecating anything, and does not propose SQLite as the
   backend any consumer should pick for production.
5. **Not a streaming-replica or HA variant.** Litestream-style
   continuous backup is out of scope; a consumer who wants durable
   multi-node storage picks Postgres.

---

## 8. Alternatives rejected

### 8.1 Keep Postgres as the local-dev backend
Rejected: Postgres requires Docker or a service install. Embedded /
in-process use (§2.1 use case 2) is not achievable. CI startup cost
is non-trivial even when Docker is available.

### 8.2 Valkey-only for local dev
Rejected: Valkey idle memory (~100 MB), network-service requirement,
and the "is valkey running?" contributor-friction axis. Embedded
use (§2.1 use case 2) not achievable for the same reason as Postgres.

### 8.3 In-tree mock backend
Rejected: a mock that doesn't exercise real SQL transactional
behaviour misses the bug class that motivates backend-integration
tests in the first place. SQLite exercises real SQL at roughly the
same cost.

### 8.4 DuckDB / sled / redb
Rejected: DuckDB is analytics-shaped, not OLTP. sled and redb are
not SQL — we would still need a mock-backend-like adapter layer.
SQLite's SQL shape matches the Postgres backend's SQL shape, so
"Postgres minus LISTEN/NOTIFY minus partitioning" is a small,
well-scoped adapter surface rather than a new storage paradigm.

### 8.5 Share migrations with Postgres (symlink or macro)
Rejected: see §5.3. Dialect gap is too wide; either the shared
migrations regress Postgres throughput by dropping partitioning, or
the macro layer has to handle arbitrary SQL translation. Hand-porting
is the honest cost.

---

## 9. Open questions

Genuine forks for owner review before acceptance.

### 9.1 SQLite version floor

SQLite `RETURNING` landed in 3.35 (2021). JSON functions landed in
3.38 (2022). Modern distros ship 3.40+; older LTS distros (Ubuntu
20.04, RHEL 8) ship 3.31 / 3.26. Three options:

- **A.** Target 3.35+ (most sqlx-pinned SQLite builds include it
  statically; contributors picking the workspace version always
  get new-enough). Reject old-distro support as acceptable cost.
- **B.** Target 3.38+ to use JSON1 ergonomically.
- **C.** Target 3.26+ and hand-roll JSON blob handling / no RETURNING.
  Highest maintenance cost; lowest barrier.

Recommendation: **A**. Needs owner approval.

### 9.2 WAL mode: mandatory or configurable?

WAL mode is the only sensible choice for a concurrent-reader workload
and for serializable-with-concurrency semantics. Making it mandatory
(always `PRAGMA journal_mode=WAL`) simplifies the backend. Making it
configurable (`FF_SQLITE_WAL=true|false`) is a one-line cost.

- **A.** Always WAL. Simpler.
- **B.** Configurable. Supports esoteric cases (shared-filesystem
  networked file, some VM setups) where WAL misbehaves.

Recommendation: **A** unless a concrete misbehaviour case surfaces.
Needs owner sign-off.

### 9.3 Test matrix scope

- **A.** Run the full `ff-test` suite against SQLite. Highest confidence;
  some tests (multi-process, capability-dependent) will need per-backend
  skips.
- **B.** Run a subset — hot path + subscribe + suspend/signal. Lower
  confidence; lower CI minutes.

Recommendation: **A**, with per-test capability-gated skips matching
the §6 deferral set. Needs owner sign-off.

### 9.4 Release target

- **A.** v0.11 — co-ships with Postgres Wave 9 (RFC-020).
- **B.** v0.12 — decoupled from Wave 9; gives Wave 9 landing room
  first.
- **C.** Separate minor (v0.11.x patch line) — lowest risk; no
  headline-feature coupling.

Recommendation: none — this is a scheduling tradeoff the owner
adjudicates. **Owner decision required.**

### 9.5 Umbrella re-export

- **A.** `flowfabric` umbrella crate re-exports `ff-backend-sqlite`
  under a `sqlite` feature (mirrors `postgres`).
- **B.** SQLite stays opt-in-only: consumers depend on
  `ff-backend-sqlite` directly; no umbrella feature.

Recommendation: **A** for consistency with Postgres. Needs owner
sign-off. **Owner decision required.**

### 9.6 Fundamental compatibility check

Survey of Postgres migrations and `crates/ff-backend-postgres/src/lib.rs`
found **no blocker**. The Postgres backend does not use advisory
locks; serialization is achieved via `SERIALIZABLE` isolation and
`FOR UPDATE`, both of which have SQLite analogues (default isolation
is serializable; `FOR UPDATE` is a no-op because SQLite writes
serialize at the file lock). Partitioning is a throughput concern,
not a correctness one — dropping it (§5.3) under the §4 single-process
envelope is sound.

The only behavioural gap worth surfacing: **NOTIFY ordering across
transactions.** Postgres guarantees `pg_notify` fires at COMMIT in
commit order; §5.2 Option A's broadcast emit fires *after* `tx.commit()`
returns, which is also commit-order but per-writer, and there is only
one writer per §5.4 — so ordering holds. If a future v2 relaxes the
single-writer constraint, ordering needs re-examination.

No fundamental blocker uncovered.

---

## 10. Migration impact

**None.** This is a purely additive backend.

- Existing `FF_BACKEND=valkey` and `FF_BACKEND=postgres` deployments
  are unchanged.
- No schema change in Postgres or Valkey backends.
- No trait change in `ff-core::engine_backend`.
- No capability matrix semantics change for the two existing backends.

Consumers adopt by setting `FF_BACKEND=sqlite` and
`FF_SQLITE_PATH=<path>|:memory:`. Capability discovery (RFC-018)
handles the per-backend parity differences automatically.

---

## 11. References

- Issue #338 — "SQLite / single-node local-mode backend" (tracking)
- RFC-010 §9.2 (archived at `avifenesh/flowfabric-archive`) — original
  mention of SQLite as a future-option backend
- RFC-012 — EngineBackend trait
- RFC-017 — Postgres backend + ff-server abstraction
- RFC-018 — capability discovery
- RFC-019 — stream-cursor subscriptions
- RFC-020 — Postgres Wave 9 (deferrals mirrored per §6)
- `crates/ff-backend-postgres/src/lib.rs` — reference durable backend
- `crates/ff-backend-postgres/migrations/*.sql` — schema baseline for
  hand-porting
- `crates/ff-server/src/config.rs` — `BackendKind` resolution, target
  of the `Sqlite` variant addition
