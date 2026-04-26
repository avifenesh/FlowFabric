# RFC-023: SQLite — dev-only backend (testing harness, Temporal-pattern)

**Status:** DRAFT
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Target release:** v0.12.0 (next content delivery after v0.11.0 Postgres Wave 9)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions), RFC-020 (Postgres Wave 9 — shipped v0.11.0), RFC-022 (parked: full-parity SQLite — superseded in scope by this RFC)
**Tracking issue:** #338

> **Draft status:** this RFC is NOT accepted. Open questions in §7 list
> genuine forks the owner must adjudicate before acceptance. Where
> "Option A vs B" or a similar fork is flagged as DECIDED in-draft
> (§3, §4.1, §4.4), the decision is the drafter's recommendation and
> remains open to owner override.

> **Supersedes (in scope) RFC-022.** RFC-022 proposed a full-parity
> SQLite backend. This RFC is narrower: **dev-only, permanently.**
> RFC-022 remains `[OPEN FOR FEEDBACK]` in `rfcs/drafts/` as a
> record of the full-parity alternative; it is NOT the execution
> path. This RFC is the execution path.

---

## 1. Summary

Add a third `EngineBackend` implementation — **SQLite** — scoped
**permanently** as a dev-only / testing backend. Concrete shape:
`FF_BACKEND=sqlite` alongside the existing `valkey` (default) and
`postgres` selectors, plus a dedicated `ff-dev` example in
`examples/` that spins a zero-config ff-server against a file or
`:memory:` SQLite in one `cargo run` invocation. Temporal's
`temporal server start-dev` is the exemplar: a first-class
single-binary dev harness that is explicitly not for production use.

The scope qualifier "dev-only" is **permanent**, not a v1 stepping
stone toward production SQLite. This RFC does NOT propose a future
path to multi-writer SQLite, cluster SQLite (rqlite / litestream),
or production SQLite recommendations. §5 lists those as permanent
non-goals and §6 engages the full-parity alternative (RFC-022
scope) as a rejected alternative.

---

## 2. Motivation

Concrete use cases:

1. **cairn-fabric `cargo test` without Docker.** Cairn pins FF
   via crates.io; their integration tests require Docker PG or
   ambient Valkey. `FF_SQLITE_PATH=:memory:` removes external
   dependencies from the `cargo test` loop.
2. **FF internal `cargo test` without Docker.** `ff-test`
   exercises backend paths against Valkey or PG (both external).
   SQLite permits `cargo test --features sqlite` on a fresh
   contributor machine with zero setup.
3. **Contributor first-clone experience.** `cargo run --example
   ff-dev` should work on a fresh clone. Today the
   `FF_BACKEND=valkey` default needs a running Valkey.
4. **CI without a shared Postgres.** Consumer matrix CI against
   a shared PG hits cross-test contamination; per-test
   `file::memory:?cache=shared` sidesteps shared state entirely.
5. **Consumer onboarding / "try FF in 60 seconds."** Today
   requires Docker PG or Valkey install. Bundled `ff-dev`
   example collapses to one `cargo run`.

### 2.1 Why "dev-only" (permanent scope)

Market scan of 14 workflow / queue / execution engines: only 3
ship production SQLite; 11 of 14 treat it as dev-only, test-only,
or absent. "Dev-only SQLite" is the proven pattern (Temporal
`start-dev` exemplar). Production SQLite is niche and carries a
large maintenance tax (§10) for a use case the target consumers
(cairn, FF SaaS deployments) do not have. The project thesis
demands backends that serve consumers well — cairn, FF internal
testing, and onboarding all want dev-only; none want production
SQLite.

---

## 3. Consumer surface — DECISION: Option A (`FF_BACKEND=sqlite`)

Three candidate surfaces were evaluated:

### 3.1 Candidate options

- **Option A — `FF_BACKEND=sqlite` + `FF_SQLITE_PATH=...`.**
  Drops into existing selector; `BackendKind::Sqlite` as third
  variant; `Server::start(config)` and
  `Server::start_with_backend(...)` unchanged. Smallest surface.
- **Option B — dedicated `ff dev` subcommand / binary.**
  Reshapes config/auth for dev ergonomics (auto-`:memory:`,
  TLS off, localhost-only, banner). Closest to Temporal.
- **Option C — library-only `ff-backend-sqlite`, no ff-server
  integration.** Consumers construct `SqliteBackend::new(path)`
  and pass to `Server::start_with_backend(...)`. Most surgical;
  least discoverable.

### 3.2 Decision: Option A + `examples/ff-dev/` layered on top

Core backend wiring is **Option A** — consistent with the
existing `FF_BACKEND=valkey|postgres` pattern, zero new config
axes, `ServerConfig::sqlite` sub-config mirrors
`ServerConfig::postgres`. Layer an `examples/ff-dev/` example on
top that invokes Option A with dev-ergonomic defaults, capturing
Option-B ergonomics (one command, zero config) without adding
a new publishable binary — avoids [release publish-list
drift](../../memory/feedback_release_publish_list_drift.md) seen
on v0.3.0 / v0.3.1. Options B and C addressed in §6.

### 3.3 Production-guard — DECISION: loud warning + `FF_DEV_MODE=1` required

A consumer must NOT accidentally run SQLite in production.
Candidates: loud-warning-only (cheap, doesn't prevent misuse),
`FF_DEV_MODE=1` required (explicit opt-in, clear signal),
hardware check (brittle, confusing, leaks infra assumptions),
docs-only (insufficient — users who don't read docs are the
cohort the guard exists to catch).

**Decision:** loud warning AND require `FF_DEV_MODE=1`.

- `FF_BACKEND=sqlite` without `FF_DEV_MODE=1` refuses to start
  with a clear error:
  ```
  error: FF_BACKEND=sqlite requires FF_DEV_MODE=1 to activate.
         SQLite is a dev-only backend; set FF_DEV_MODE=1 to
         acknowledge, or pick FF_BACKEND=valkey | postgres for
         production.
  ```
- `FF_BACKEND=sqlite` with `FF_DEV_MODE=1` starts with a loud
  startup banner (WARN level, visible in JSON logs):
  ```
  WARN  FlowFabric SQLite backend active (FF_DEV_MODE=1).
  WARN  This backend is dev-only; single-writer, single-process,
  WARN  not supported in production. See RFC-023.
  ```
- The banner prints on every process start so log aggregators
  can alert on it if a SQLite backend ever reaches an environment
  it shouldn't.

This matches the `BACKEND_STAGE_READY` precedent in
`crates/ff-server/src/server.rs` (§server.rs:33) — we already
refuse unready backends at `Server::start_with_metrics`; SQLite
takes a parallel gate.

Hardware checks and docs-only are addressed in §6 as rejected
alternatives to the guard design.

### 3.4 Trait surface

**Zero new trait methods.** `SqliteBackend` implements the
existing `EngineBackend` surface; capability flags (RFC-018) are
identical to Postgres v0.11 per §4.3.

---

## 4. Complete design (whole feature, permanent dev-only scope)

### 4.1 Schema port strategy — DECISION: hand-ported SQLite-dialect migrations + forked runtime query modules

PG migrations `0001` … `0014` cannot be shared. Dialect gap:

| Postgres feature used                     | SQLite support                               |
|-------------------------------------------|----------------------------------------------|
| `PARTITION BY HASH (partition_key)` × 256 | **None** — drop partitioning entirely        |
| `DO $$ BEGIN FOR ... LOOP ... END $$`     | **None** — replaced by pre-enumerated DDL    |
| `BIGSERIAL` / `GENERATED ALWAYS AS IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT`       |
| `jsonb` + `jsonb_build_object` / `jsonb_set` | `TEXT` + JSON1 `json_object` / `json_patch` |
| `BYTEA`                                   | `BLOB`                                       |
| `CREATE TRIGGER ... PERFORM pg_notify(...)` | **None** — replaced by §4.2 in-proc channels |
| `FOR UPDATE SKIP LOCKED`                  | Tautological under single-writer — plain `SELECT` + serializable default |
| `ON CONFLICT ... DO UPDATE`               | Supported (3.24+)                            |
| `RETURNING`                               | Supported (3.35+) — §7.1                     |
| GIN index on `text[]`                     | JSON1 + table scan (dev-only envelope)       |

**Migrations:** hand-ported SQLite-dialect files in
`crates/ff-backend-sqlite/migrations/0001_*.sql` … `0014_*.sql`,
1:1 numbered with PG for parity-drift detection (count
mismatch = lint-fail; §7.3). Partitioning is **dropped** —
one non-partitioned table per entity. Correct under the
single-writer / dev-throughput envelope; not correct for PG,
which is why PG partitions.

**Runtime SQL (the ~10 inline `jsonb_build_object` / `jsonb_set`
sites in the PG backend):** options: (a) fork the Rust
query-module per backend (duplicate strings); (b) per-query
`dialect!` macro rewriting PG → SQLite; (c) `Dialect` trait
abstraction.

**Decision:** (a) — fork the Rust query-module. PG backend is
1343 LOC; SQLite ballpark-same. Two copies of straightforward
SQL is a known-cost tax (§10) the owner accepted. Macros add a
translation layer that is itself a bug surface; trait
abstractions trade surgical edits for architectural edits on
every PG RFC. Dev-only backend does not need to promise
performance parity; simplicity of (a) wins.

### 4.2 Pub/sub replacement

PG uses `pg_notify` triggers (migrations 0001, 0006, 0007 +
`fn_notify_*`) with `LISTEN` in-process. SQLite has no
equivalent. **Replacement:** `tokio::sync::broadcast` channels
on `SqliteBackend`, one per subscription type
(`subscribe_lease_history`, `subscribe_completion`,
`subscribe_signal_delivery`, RFC-019 stream frames). Write paths
emit on the broadcast channel after `tx.commit()` returns;
subscribers hold `broadcast::Receiver` and fan out to the
existing `Stream` surface (RFC-019).

**Permanent shape:** in-process only. Cross-process pub/sub is
a **permanent non-goal** (§5). No v2 hedge. Consumers needing
cross-process semantics pick Valkey or PG — that is the entire
point of having three backends.

Ordering: PG guarantees `pg_notify` fires at COMMIT in commit
order. Broadcast emit fires after `tx.commit()` returns,
per-writer, and there is exactly one writer (§4.6). Commit-order
fan-out preserved by construction.

### 4.3 Parity commitment

**SQLite parity = PG v0.11 parity.** Every RFC-018 `Supports`
flag on SQLite equals the same flag on PG at v0.11: Wave 9
methods (change_priority, replay_execution, cancel_flow_header,
ack_cancel_member per RFC-020) supported; pre-Wave-9 hot path
(create / claim / complete / fail / suspend / signal /
subscribe_* / timeout scanners / budget quota) supported; no
`EngineError::Unavailable` gaps beyond what PG v0.11 has (none).

**Dev-only qualifier means:** NOT Wave-10+ until PG equivalent
ships (same release discipline); NOT production-scale throughput
— the ~10³ write-QPS single-writer cap is a resource bound, not
a parity gap.

**Test coverage:** full `ff-test` suite on SQLite. No
"subset of tests" carve-out. Per-test skips only via RFC-018
capability flags, never via backend-identity checks.

### 4.4 Embedded entry-point — DECISION: Option Y (no new API)

Do consumers want `ff-server` as a library with no HTTP
listener?

- **Option X:** add `Server::start_embedded(backend: Arc<dyn
  EngineBackend>) -> ServerHandle` — no HTTP bind.
- **Option Y:** no new API. `Server::start_with_backend(...)`
  already exists at `crates/ff-server/src/server.rs:677` and
  accepts any `Arc<dyn EngineBackend>`. No-HTTP consumers
  construct `SqliteBackend::new(path).await?` directly and
  route through `ff-engine`, bypassing ff-server entirely.

**Decision:** **Option Y**. `start_with_backend` already covers
library-level use; `start_embedded` duplicates it with marginally
different defaults. `SqliteBackend::new(path)` + `ff-engine`
suffices for no-HTTP embedded. Documenting this as the embedded
story is cheaper than a third `start_*` method.

**Consequence:** `SqliteBackend::new(path: &str) -> Result<Self,
BackendError>` is a **public API** stability commitment. The
embedded path skips `ServerConfig` entirely, so TLS / auth / HTTP
bind concerns stay contained in ff-server.

### 4.5 Production-guard mechanism

Per §3.3. Implementation locus: `Server::start_with_metrics`
(`crates/ff-server/src/server.rs`), parallel to the existing
`BACKEND_STAGE_READY` gate at line 33. On
`BackendKind::Sqlite`: check `FF_DEV_MODE=1`, return
`ServerError::SqliteRequiresDevMode` if absent; otherwise emit
the §3.3 warn banner on startup. `SqliteBackend::new(path)`
called directly (embedded path, §4.4 Y) also emits the banner
on construction so the dev-only signal survives the no-ff-server
case.

### 4.6 Test infrastructure

- **`:memory:` mode.** Per-test pool with
  `file:ff-test-<uuid>?mode=memory&cache=shared` URI so 1 writer
  + N readers share state within the pool. `sqlx::migrate!` runs
  on pool-init; no persistence across restarts.
- **File mode.** `FF_SQLITE_PATH=/tmp/ff-dev.db`. Idempotent
  migrations on pool-init (sqlx's migration table).
- **Parallel-test isolation.** Per-test unique DB name via UUID
  in the URI — each test is a separate in-memory instance, no
  cross-test contamination.
- **Connection pool.** `sqlx::SqlitePool` with 1 writer + N
  readers (N=4; `FF_SQLITE_POOL_SIZE` override). WAL default on
  (`PRAGMA journal_mode=WAL` in the connect hook). No-op under
  `:memory:`; load-bearing for reader concurrency under file.

### 4.7 `cargo test` story for cairn-fabric

Example of a cairn migration from Docker-PG to SQLite-dev:

```rust
// cairn-fabric/tests/integration_sqlite.rs
use ff_backend_sqlite::SqliteBackend;
use ff_server::Server;
use std::sync::Arc;

#[tokio::test]
async fn end_to_end_op_roundtrip_on_sqlite() {
    let db_uri = format!(
        "file:cairn-test-{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4(),
    );
    let backend = SqliteBackend::new(&db_uri).await.unwrap();
    let server = Server::start_with_backend(
        test_config(),
        Arc::new(backend),
    ).await.unwrap();
    let client = Client::connect(server.http_addr()).await.unwrap();
    // ... cairn's existing test logic, unchanged ...
}
```

Before: `docker compose up postgres` + `FF_POSTGRES_URL` +
per-test schema isolation. After: 20 lines and `cargo test`.

---

## 5. Non-goals (permanent, not deferred)

These are **permanent** non-goals. No future RFC is expected to
lift them; lifting any of these is a scope-thesis change, not a
roadmap item.

1. **NOT production-scale SQLite.** The ~10³ write-QPS ceiling,
   the single-writer cap, and the single-process envelope are
   inherent. Production consumers pick Valkey or Postgres.
2. **NOT multi-writer SQLite.** No WAL-over-NFS, no
   synchronized-filesystem setups, no "what if two ff-server
   processes share a file" engineering. Unsupported.
3. **NOT clustered SQLite.** No rqlite, no dqlite, no
   replication layer. A consumer who wants multi-node durable
   picks Postgres.
4. **NOT streaming-replica / HA.** No Litestream-style
   continuous backup. Dev data is either `:memory:` (ephemeral)
   or a local file (user-managed).
5. **NOT cross-process pub/sub.** The §4.2 in-process
   broadcast channel is the permanent shape.
6. **NOT a replacement for Valkey or Postgres recommendations.**
   Default remains `FF_BACKEND=valkey`. No deprecation of any
   existing backend.
7. **NOT v2 expansion.** No "dev-only SQLite today, production
   SQLite tomorrow" path. Production SQLite is out of scope
   permanently. If a consumer articulates a genuine production
   single-node use case in the future, that is a new RFC with
   a new scope thesis — not a v2 of this one.

---

## 6. Alternatives rejected (honest engagement)

### 6.1 RFC-022 scope — full-parity SQLite (including production)

**Rejected permanently.** Market scan (§2.1): only 3 of 14
engines ship production SQLite. The maintenance tax of keeping
a full-parity SQLite through every schema-changing and
trait-surface RFC compounds over time. Target consumers
(cairn, FF internal testing, onboarding) want `cargo test`
without Docker, not production SQLite. RFC-022 remains parked
`[OPEN FOR FEEDBACK]` as the record of the full-parity
alternative; RFC-023 is the execution path.

### 6.2 `pg_tmp` / `embedded-postgres`

Third-party Rust crates bundle Postgres binaries per-process.
Rejected: maintenance burden (per-target-triple binary
distribution, trails upstream PG by weeks), ~100+ MB bundled
per triple, seconds-not-milliseconds startup, and the fidelity
gain over SQLite is marginal for what cairn actually tests
(FlowFabric semantics, not PG quirks). SQLite's sub-ms
`:memory:` startup and zero-binary-bundle cost dominate.

### 6.3 `docker-compose.dev.yml` + Postgres

Cheapest partial: shared Docker PG for cairn + contributors.
Rejected: Docker-required onboarding blocks contributors
without Docker (Apple Silicon pre-Rosetta, Windows sans WSL2,
bare CI runners); shared service imposes per-binary schema
isolation ceremony; seconds of fixed Docker startup per CI job.
SQLite is the cheaper complete solution for dev-only scope.

### 6.4 Per-schema isolation in shared Postgres

Cheapest CI-only alternative. Rejected: shared-service
dependency (outage cascades to test pass rate), schema
lifecycle management failure-prone (leaked schemas accumulate),
and does not address the first-clone `cargo test` story
(contributors still need PG). Does not cover §2.1 cases 1, 3, 5.

### 6.5 PGlite (Wasm Postgres)

Rejected: Rust integration story immature (JS/Wasm project, no
production-quality Rust shim as of 2026-04), Wasm runtime
overhead, high-ceremony upstream tracking for a low-stakes use
case. SQLite via sqlx is a decade-hardened Rust-native path.

### 6.6 In-tree mock `EngineBackend`

A hand-rolled `HashMap`-backed mock. Rejected: misses the real
SQL transactional bug class, mock semantics drift from real
backends over time, and hand-rolling ~90 methods with mutable
state + locking is a small backend's-worth of code itself — not
cheaper than SQLite.

### 6.7 Hardware-check production-guard

Rejected: §3.3. Brittle (beefy dev box trips the check),
confusing (opaque refusal), leaks infra assumptions.

### 6.8 Docs-only production-guard

Rejected: §3.3. Consumers who don't read docs are the cohort
the guard exists to catch.

### 6.9 Option B — `ff dev` subcommand binary

Rejected for core wiring, retained as the `examples/ff-dev/`
ergonomic layer (§3.2). Reasoning: a new binary in the
publishable crate list adds release-publish-list drift risk (see
[feedback_release_publish_list_drift](../../memory/feedback_release_publish_list_drift.md));
an unpublished example delivers the same ergonomics at lower
release-infra cost.

### 6.10 Option C — library-only, no ff-server integration

Rejected: §3.2. Losing `FF_BACKEND=sqlite` cuts the discovery
path that uses cases 1, 2, 3, 4 depend on (they all want a
working ff-server, not just a library).

### 6.11 Option X — `Server::start_embedded` new API

Rejected: §4.4. `Server::start_with_backend(...)` already
exists and covers the embedded case.

---

## 7. Open questions (genuine forks for owner adjudication)

### 7.1 SQLite version floor

SQLite `RETURNING` landed in 3.35 (2021); JSON1 is default-built
in 3.38 (2022); modern distros ship 3.40+; Ubuntu 22.04 ships
3.37, RHEL 8 ships 3.26. Options:

- **A.** 3.35+ — usable `RETURNING`, JSON1 via explicit
  `PRAGMA` where needed. Ubuntu 22.04 contributors need a
  newer sqlite (or the sqlx-bundled build).
- **B.** 3.38+ — JSON1 fully ergonomic. Narrower distro
  coverage.
- **C.** 3.45+ — latest `json_patch` / strict mode. Narrowest.

Drafter recommends **A** with sqlx's `sqlite` feature using the
**bundled build** (statically links SQLite into the binary, so
distro version is irrelevant for consumers). This decouples
SQLite version from distro; contributors on Ubuntu 22.04 get
the bundled version automatically. Open for owner decision —
bundled-build has a per-target-triple compile-time cost and
owner should weigh that.

### 7.2 Crate publishable-list posture

- **A.** `ff-backend-sqlite` is published to crates.io (like
  `ff-backend-postgres`, `ff-backend-valkey`), gated behind an
  umbrella `flowfabric/sqlite` feature.
- **B.** `ff-backend-sqlite` is in-workspace but `publish = false`
  (consumers who want it depend on git or do not get it via
  crates.io). Lowers publish-list drift surface; loses
  discoverability.
- **C.** Published but NOT re-exported by the `flowfabric`
  umbrella crate — consumers explicitly `cargo add
  ff-backend-sqlite` with its own version. Middle ground.

Drafter recommends **A** for consistency with the other two
backends. Open for owner because every new publishable crate is
a release-list line item ([feedback_release_publish_list_drift](../../memory/feedback_release_publish_list_drift.md));
owner should weigh the release-discipline cost against the
discoverability gain.

### 7.3 Scope of migrations 1:1 parity-drift linting

Should CI lint that
`crates/ff-backend-sqlite/migrations/NNNN_*.sql` exists for
every `crates/ff-backend-postgres/migrations/NNNN_*.sql`? Drafter
recommends **yes** — a simple shell-based lint in `scripts/`
prevents a PG RFC from landing a migration and forgetting the
SQLite port. Open because "forgetting the port" might be
intentional (a PG-only admin operation) — need a small
allow-list mechanism (`.sqlite-skip` sidecar) for those.

---

## 8. Migration impact (consumer-facing)

**Purely additive.** Existing consumers unaffected:

- `FF_BACKEND=valkey` (default) and `FF_BACKEND=postgres` behave
  identically to v0.11.
- No schema change in Postgres or Valkey backends.
- No trait change in `ff-core::engine_backend`.
- No capability-matrix change for the two existing backends.

Consumer opt-in path (cairn example):

1. `cargo add ff-backend-sqlite` (or enable `flowfabric/sqlite`
   feature, per §7.2).
2. Set `FF_BACKEND=sqlite` + `FF_DEV_MODE=1` +
   `FF_SQLITE_PATH=:memory:` in the test harness env.
3. Replace `docker compose up postgres` preamble with nothing.
4. `cargo test` — passes.

No code change in consumer test bodies beyond the `ServerConfig`
wiring (which most consumers already parameterize over
`FF_BACKEND`).

---

## 9. Release readiness (hard gates — NOT v1/v2 split)

All of the following must be satisfied before v0.12.0 ships.
There is no "v1 slice now, rest later" — the feature ships
whole or does not ship.

- [ ] All 3 backends (Valkey, Postgres, SQLite) pass the full
      `ff-test` suite. Per-test skips only via RFC-018 capability
      flags, not backend-identity checks.
- [ ] cairn-fabric successfully migrates at least one integration
      test file from Docker-PG to SQLite-dev; migration PR merged
      into cairn-fabric tree.
- [ ] Root `README.md` updated with a "Local dev in 60 seconds"
      section that runs the `ff-dev` example (per §3.2).
- [ ] `scripts/smoke-sqlite.sh` exists, mirrors the shape of
      `scripts/smoke-v0.7.sh` / `scripts/published-smoke.sh`, and
      passes.
- [ ] `scripts/published-smoke.sh` extended to include a SQLite
      scenario — scratch consumer `cargo add flowfabric` with the
      `sqlite` feature, runs the 20-line cairn-example body, must
      pass before tag (per [feedback_smoke_before_release](../../memory/feedback_smoke_before_release.md)).
- [ ] Docs: new `docs/dev-harness.md` that enumerates
      dev→prod gotchas (SQLite absent in prod, `FF_DEV_MODE=1`
      required, single-writer envelope, migration port parity).
- [ ] `examples/ff-dev/` example compiles + runs + documents
      "try it in 60 seconds."
- [ ] RELEASING.md updated if §7.2 lands as Option A (new
      publishable crate).
- [ ] `release.yml` + `release.toml` updated if new publishable
      crate.

Per [feedback_release_publish_list_drift](../../memory/feedback_release_publish_list_drift.md),
release-config files (release.yml, release.toml, RELEASING.md)
are updated in the same PR that introduces the crate.

---

## 10. Maintenance tax commitment

Honest enumeration:

- **Schema-changing RFCs port 1 more migration per PR.** §7.3
  parity lint enforces.
- **Trait-surface RFCs add a third backend impl per PR.** Same
  discipline as ff-backend-valkey; SQLite is row 3.
- **CI matrix.** One new cell `cargo test --workspace --features
  sqlite`; ~2 min additional wall clock (SQLite fast; compile
  dominates).
- **Parity-drift detection.** §7.3 lint + existing
  `tests/capabilities.rs` pattern (RFC-018).
- **Bundled SQLite binary size.** sqlx bundled build adds
  ~500 KB; feature-gated, non-opt-in consumers don't pay.
- **Postgres-RFC cognitive load.** Third-backend consideration
  on every PG PR; valkey already precedents this, RFC-017
  through RFC-020 landed without blockage.

**Owner has accepted this tax** per the "SQLite for testing"
directive. This section is the honest ledger, not a plea for
re-evaluation.

---

## 11. References

- Issue **#338** — tracking
- **RFC-022** (parked, `rfcs/drafts/RFC-022-sqlite-backend.md`) —
  full-parity alternative, superseded in scope
- **RFC-012** EngineBackend trait; **RFC-017** PG backend + ff-server
  abstraction; **RFC-018** capability discovery; **RFC-019** stream-cursor
  subscriptions; **RFC-020** PG Wave 9 (shipped v0.11.0 2026-04-26)
- **Temporal** `temporal server start-dev` — exemplar dev-only
  embedded-DB pattern
- `crates/ff-backend-postgres/src/lib.rs` — reference (1343 LOC)
- `crates/ff-backend-postgres/migrations/0001_*.sql` …
  `0014_*.sql` — schema baseline for hand-porting
- `crates/ff-server/src/server.rs:33` — `BACKEND_STAGE_READY`
  gate precedent (§4.5)
- `crates/ff-server/src/server.rs:677` — `Server::start_with_backend`
  (§4.4 Y)
- `crates/ff-server/src/config.rs:13` — `BackendKind` enum
  (gains `Sqlite` per §3)
- `scripts/published-smoke.sh` — §9 release-gate extension target
- Market scan of 14 engines (Temporal, Hatchet, Prefect, Inngest,
  Trigger, Windmill, Argo, Cadence, Conductor, Zeebe, Restate,
  DBOS, DataDog Workflows, Airbyte) — dev-only embedded pattern
  is majority; production SQLite minority (Hatchet, Prefect
  single-tenant, niche others).
