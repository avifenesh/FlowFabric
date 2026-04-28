# FlowFabric dev harness — SQLite backend (RFC-023)

`FF_BACKEND=sqlite` is a **dev-only** backend for local development
and `cargo test` without Docker. This page is the operator +
contributor guide: how to wire it up, the dev→prod gotchas, and the
production guard that keeps it off real deployments.

Not a deployment target. If you are standing up a production
FlowFabric stack, read [`DEPLOYMENT.md`](DEPLOYMENT.md) instead.

---

## 1. Positioning

**SQLite is a testing harness; Valkey is the engine; Postgres is the
enterprise persistence layer.** See
[`rfcs/RFC-023-sqlite-dev-only-backend.md`](../rfcs/RFC-023-sqlite-dev-only-backend.md)
§1.0 for the full positioning statement. The scope qualifier is
permanent — there is no "dev-only today, production-SQLite tomorrow"
path.

Primary use cases:

1. **cairn-fabric `cargo test` without Docker.** `FF_SQLITE_PATH=:memory:`
   removes Docker Postgres / ambient Valkey from the test loop.
2. **FlowFabric contributor first-clone experience.** `cargo run
   --example ff-dev` works on a fresh machine with zero external
   services.
3. **CI without a shared Postgres.** Per-test
   `file:ff-test-<uuid>?mode=memory&cache=shared` sidesteps shared-
   schema contamination.

---

## 2. Canonical setup

### 2.1 `FF_DEV_MODE=1` — required

Both the ff-server HTTP branch and the library-level
`SqliteBackend::new` refuse to construct unless `FF_DEV_MODE=1` is
set in the environment. This is the production-guard gate (RFC-023
§3.3). There is no way to disable it short of editing the backend
source.

Without it, `SqliteBackend::new` returns
`BackendError::RequiresDevMode` carrying the message:

```
SqliteBackend requires FF_DEV_MODE=1 to activate. SQLite is
dev-only; see https://github.com/avifenesh/FlowFabric/blob/main/docs/dev-harness.md
for details.
```

And `ff-server` with `FF_BACKEND=sqlite` and no `FF_DEV_MODE=1`
refuses to start:

```
error: FF_BACKEND=sqlite requires FF_DEV_MODE=1 to activate.
       SQLite is a dev-only backend; set FF_DEV_MODE=1 to
       acknowledge, or pick FF_BACKEND=valkey | postgres for
       production.
```

### 2.2 Canonical location: `.cargo/config.toml`

Set `FF_DEV_MODE=1` (and any other dev harness vars) in a
`[env]` block in `.cargo/config.toml` at the workspace root. This
survives across parallel `cargo test` invocations, across workspace
members, and across editor-integrated test runners.

```toml
# .cargo/config.toml
[env]
FF_DEV_MODE      = "1"
FF_BACKEND       = "sqlite"
FF_SQLITE_PATH   = ":memory:"
```

### 2.3 Do NOT use `std::env::set_var` in test bodies

Rust 2024 marks `std::env::set_var` as **`unsafe`** because the
process environment is shared global mutable state and is racy with
parallel threads. `cargo test` runs tests in parallel by default.

If you absolutely need to set an env var in a test body, understand
that:

- The value leaks to every other parallel test in the same process.
- Two tests both calling `set_var` on the same key race; whichever
  wins last sticks until another test clobbers it.
- Reading the value from another thread while `set_var` runs is UB
  under Rust 2024.

Prefer `.cargo/config.toml [env]`. If unavoidable (e.g. a
one-off test gating on a unique value), serialize the test with
`#[serial_test::serial]` and document the race.

### 2.4 Embedded (cairn-canonical) example

```rust
// tests/integration_sqlite.rs
use ff_sdk::{FlowFabricWorker, WorkerConfig, SqliteBackend};
use std::sync::Arc;

#[tokio::test]
async fn roundtrip_on_sqlite() {
    // FF_DEV_MODE=1 is set in .cargo/config.toml [env] — no set_var needed.

    let uri = format!(
        "file:mytest-{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4(),
    );
    let backend = Arc::new(SqliteBackend::new(&uri).await.expect("sqlite init"));

    let config = WorkerConfig::builder()
        .lanes(vec!["default".into()])
        .build()
        .expect("worker config");

    let worker = FlowFabricWorker::connect_with(config, backend.clone(), None)
        .await
        .expect("worker connect");

    // Drive ops through the backend trait directly. The
    // claim/signal convenience methods (`claim_next`, etc.) are
    // valkey-default-gated and absent under sqlite-only features.
    worker.backend().ping().await.unwrap();
}
```

Feature posture in your consumer `Cargo.toml`:

```toml
[dev-dependencies]
ff-sdk            = { version = "0.12", default-features = false, features = ["sqlite"] }
ff-backend-sqlite = "0.12"
```

---

## 3. Connection-URI modes

### 3.1 `:memory:?cache=shared`

Ephemeral; database vanishes when the last connection closes. Each
`sqlx::SqlitePool` owns multiple connections; `cache=shared` lets
those connections see each other's state within the pool. No
persistence across process restarts.

```rust
SqliteBackend::new(":memory:?cache=shared").await?
```

### 3.2 `file:<name>?mode=memory&cache=shared` — per-test isolation

For parallel tests, each test body constructs its own unique named
in-memory DB:

```rust
let uri = format!(
    "file:ff-test-{}?mode=memory&cache=shared",
    uuid::Uuid::new_v4(),
);
let backend = Arc::new(SqliteBackend::new(&uri).await?);
```

Each UUID names a separate database instance; tests cannot cross-
contaminate. The `OnceCell`-backed registry (`SqliteBackend::new`
caches one handle per canonicalised URI) ensures multiple
`new(uri)` calls for the same UUID share a single backend instance —
this is what preserves in-process pub/sub wakeup semantics for
subscribe-aware tests.

### 3.3 File-backed — `/tmp/ff-dev.db` or project-local

Use a file path for a dev harness that persists across `cargo run`
invocations:

```bash
FF_DEV_MODE=1 FF_BACKEND=sqlite FF_SQLITE_PATH=/tmp/ff-dev.db cargo run -p ff-server
```

WAL mode is enabled by default (`PRAGMA journal_mode=WAL` in the
connect hook). Reader concurrency is load-bearing here; `:memory:`
mode no-ops the pragma.

---

## 4. dev → prod gotchas (what SQLite does NOT do)

SQLite's dev-only scope is permanent (RFC-023 §5). These are the
invariants that do **not** port from SQLite up to a production
Valkey or Postgres deployment.

### 4.1 No partitioning

The Postgres backend hash-partitions flow + execution tables 256
ways (`PARTITION BY HASH (partition_key) × 256`). The SQLite
backend drops partitioning entirely — one non-partitioned table per
entity — because SQLite does not support `PARTITION BY` and single-
writer semantics make partitioning tautological.

**Impact:** the scanner supervisor collapses to `N=1` on SQLite (one
tick task per reconciler, no partition fan-out). Workloads that
would stress per-partition isolation on Postgres cannot exercise
that dimension on SQLite.

### 4.2 No cross-process pub/sub

`subscribe_lease_history` / `subscribe_completion` /
`subscribe_signal_delivery` use `tokio::sync::broadcast` channels
on the `SqliteBackend` instance — in-process only. A second
ff-server process pointing at the same file will **not** receive
subscribe events originated elsewhere.

**Impact:** cannot exercise multi-process subscribe fan-out on
SQLite. Use Valkey or Postgres for that. This is a permanent
non-goal (RFC-023 §5 #5).

### 4.3 Single-writer

SQLite is single-writer by construction. The retry classifier
(`is_retryable_sqlite_busy`) absorbs `SQLITE_BUSY` / `SQLITE_LOCKED`
bursts under parallel-test load, but sustained write contention
will slow the backend down.

**Impact:** throughput ceiling ~10³ write QPS. Production scale
demands Valkey or Postgres.

### 4.4 No cluster, no replication, no HA

No rqlite, no dqlite, no Litestream, no WAL-over-NFS. Dev data is
either `:memory:` (ephemeral) or a local file (user-managed).

### 4.5 Parity is capability-matrix-exact, not cost-equivalent

Every `Supports` flag reported by `SqliteBackend::capabilities()`
matches the Postgres v0.11 flag set (see
[`POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md)). Methods
that are `Supports::X = true` on Postgres are `true` on SQLite too
— if the flag reads true, the op works. The exception is
`Supports::claim_for_worker`, which is `false` on SQLite (no
scheduler wired — RFC-023 §5 non-goal); `subscribe_instance_tags`
is `n/a` on all three backends per #311.

Capability parity does **not** mean perf parity. SQLite is ~10³
QPS dev-envelope; Postgres + Valkey are production-scale.

---

## 5. `FF_DEV_MODE` vs the other dev-leaning env vars

FlowFabric has two pre-existing dev-leaning knobs:

- `FF_ENV=development` — enables generic dev-mode shortcuts across
  the stack.
- `FF_BACKEND_ACCEPT_UNREADY=1` — overrides the
  `BACKEND_STAGE_READY` gate for backends still in staging.

`FF_DEV_MODE=1` is **orthogonal** to both. It is SQLite-specific,
does nothing on `FF_BACKEND=valkey|postgres`, and is not an alias
for either of the two knobs above. See RFC-023 §3.3 for the
separation rationale.

SQLite joins `BACKEND_STAGE_READY` at v0.12 introduction (no
stage-E ramp), so the generic `FF_BACKEND_ACCEPT_UNREADY=1`
override is not needed for the SQLite path.

---

## 6. Reference example

See [`examples/ff-dev/`](../examples/ff-dev/) — the v0.12 dev-
harness example that spins a zero-config ff-server against an
in-memory SQLite in one `cargo run` invocation. Temporal's
`temporal server start-dev` is the exemplar for this shape.

---

## 7. Troubleshooting

- **`BackendError::RequiresDevMode`**. Set `FF_DEV_MODE=1` in
  `.cargo/config.toml [env]` (preferred) or the shell running
  `cargo test`.
- **`SQLITE_BUSY` storms under parallel tests.** The retry wrapper
  absorbs these up to `MAX_ATTEMPTS = 3`. If you see them surface
  as hard errors, check for test bodies holding an explicit
  transaction across `await` points longer than intended.
- **Subscribe test sees no events across `SqliteBackend` handles.**
  Check that the two `new(uri)` calls use the **same** URI — the
  per-path registry returns a shared handle only when URIs
  canonicalise identically. `:memory:` URIs without a unique name
  count as distinct DBs by construction.
- **WAL file (`*.db-wal`, `*.db-shm`) left behind after crash.**
  Safe to delete when the main `.db` is quiescent; on re-open SQLite
  will recreate them as needed. Do not delete while the backend is
  live.
- **Stray `FF_HOST` / `FF_PORT` / `FF_CONNECTION_URL` in environment.**
  Under `ff-sdk = { default-features = false, features = ["sqlite"] }`
  the Valkey-dialing code path is compiled out, so stray env vars
  are inert. Under `valkey-default` they still influence
  `FlowFabricWorker::connect`; `connect_with` bypasses them.
