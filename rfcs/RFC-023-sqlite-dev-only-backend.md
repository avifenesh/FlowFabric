# RFC-023: SQLite — dev-only backend (testing harness, Temporal-pattern)

**Status:** ACCEPTED
**Revision:** 5
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Accepted:** 2026-04-26
**Target release:** v0.12.0 (next content delivery after v0.11.0 Postgres Wave 9)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions), RFC-020 (Postgres Wave 9 — shipped v0.11.0), RFC-022 (parked: full-parity SQLite — superseded in scope by this RFC)
**Tracking issue:** #338

### Revision 5 summary (2026-04-26)

Phase 1a impl agent surfaced that Revision 4's §4.4 item 10 ("one
`#[cfg(feature = "valkey-default")]` line removed, zero behavior
change") was wrong against ground truth. The real code surface:

- `crates/ff-sdk/src/lib.rs:93-94` gates the `worker` MODULE itself
  (`#[cfg(feature = "valkey-default")] pub mod worker;`), not only
  the re-export — so removing the re-export gate alone is a no-op
  under sqlite-only features (the module is absent).
- `crates/ff-sdk/src/worker.rs:6` carries an unconditional
  `use ferriskey::{Client, Value};` at module scope; `Client` is
  embedded in the `FlowFabricWorker` struct at `worker.rs:63`; the
  type cannot be constructed without dialing Valkey.
- `FlowFabricWorker::connect_with` at `worker.rs:587-596` literally
  calls `Self::connect(config).await?` first and then swaps the
  backend — the opposite of a backend-agnostic entry point.
- `claim_from_grant` / `claim_via_server` / `claim_from_reclaim_grant`
  / `claim_resumed_execution` / `claim_execution` /
  `read_execution_context` / `deliver_signal` all use
  `self.client` (ferriskey) directly and are NOT gated today
  (only `claim_next` + its helpers are `direct-valkey-claim`-gated).

Owner decision 2026-04-26: do the worker.rs surgery in Phase 1a
(Option C), not a cfg-line-removal narrow fix. §4.4 item 10 is
rewritten to honestly enumerate the scope; §4.7.1's stray-env
footnote is replaced with a post-surgery compile-surface note;
§8 absorbs the additional minor-breaking-change surface (claim
methods become `valkey-default`-gated at the API boundary); §9
adds a `cargo check -p ff-sdk --no-default-features --features sqlite`
CI cell as the mechanical regression guard; §10 adds the
cfg-regression maintenance-tax line. No open questions added;
no reframing of §§1–3, §4.1–§4.3, §4.5–§4.7.2, §5, §6, §7.

### Revision 4 summary (2026-04-26)

Post-acceptance Round-3 extra-sanity review yielded 6 structural
findings across Reviewers A (technical) and B (consumer); Reviewer
C was ACCEPT-as-is. No scope changes; each edit traces to a numbered
finding:

- **A1 (§4.4, §4.7.1).** Committed disposition (a) — the
  `FlowFabricWorker` re-export at `crates/ff-sdk/src/lib.rs:149` must
  move out of the `valkey-default` cfg gate to remain addressable
  under cairn's `default-features = false, features = ["sqlite"]`
  dependency posture. Added as a §4.4 item and footnoted in §4.7.1.
- **A2 (§4.2).** `subscribe_completion` parity with PG v0.11 requires
  porting outbox migrations `0006_lease_event_outbox.sql`,
  `0007_signal_event_outbox.sql`, `0010_operator_event_outbox.sql`
  to SQLite (INSERT paths intact). Broadcast channel is WAKEUP only;
  cursor-resume replay goes through the outbox per RFC-019 contract.
- **A3 (§3.3, §4.5).** Resolved dual-emission ambiguity: banner
  emits ONLY from `SqliteBackend::new` (the TYPE-level guard per
  §4.5); the server branch detects construction failure and maps
  the error but does not emit its own banner.
- **B1 (§8, §4.4).** Committed disposition (a): `ServerError` at
  `crates/ff-server/src/server.rs:90` and `ServerConfig` at
  `crates/ff-server/src/config.rs:103` are NOT `#[non_exhaustive]`
  today; both gain the attribute in the same v0.12.0 PR that adds
  the new variant/field. §8 rewritten honestly — this is a minor
  breaking change for exhaustive-match and struct-literal consumers.
- **B2 (§4.5).** Committed exact embedded-path error text to parity
  with the §3.3 server-path text; both carry the docs URL.
- **B3 (§9, §4.7.1).** `docs/dev-harness.md` content-spec tightened
  to name `.cargo/config.toml [env]` as canonical; added §4.7.1
  footnote warning that stray `FF_HOST` / `FF_PORT` /
  `FF_CONNECTION_URL` env must be unset because
  `FlowFabricWorker::connect_with` at `crates/ff-sdk/src/worker.rs:587-590`
  internally dispatches through `connect` before backend swap.

Plus two non-blocking C-trivia inclusions:

- **C-trivia-1 (§4.4 item 4).** `sqlite_dev()` disables ALL auth
  axes — worker token, admin token, waitpoint HMAC — stated
  explicitly.
- **C-trivia-2 (§4.1).** `.sqlite-skip` sidecar format: one path
  per line, `#` comments, optional `# issue:NNN` suffix.

### Revision 3 summary (2026-04-26)

Round-2 yielded A+C ACCEPT, B REQUEST-REVISION with 3 surgical
items. No scope changes; doc-only alignment to ground-truth code:

- **B-R3-1 (§4.7.1).** Replaced fictional `ff_sdk::worker::Worker`
  import with the real `ff_sdk::FlowFabricWorker` re-export at
  `crates/ff-sdk/src/lib.rs:149`, and updated the example's
  `connect_with` call-site + the "directly wraps" prose line.
- **B-R3-2 (§4.5).** Clarified that `BackendError::RequiresDevMode`
  is a NEW variant on the existing `#[non_exhaustive]`
  `BackendError` enum at `crates/ff-core/src/engine_error.rs:488`
  — NOT a new `BackendErrorKind` variant (that enum is scoped to
  Valkey transport classification per its rustdoc).
- **B-R3-3 (§9).** Added `CHANGELOG.md` `[Unreleased]` gate bullet
  for the four new public APIs plus the new
  `BackendError::RequiresDevMode` variant, per CLAUDE.md §5.

### Revision 2 summary (2026-04-26)

Round-1 produced 17 concrete findings across three reviewers
(A: technical, B: consumer, C: framing). None reframe scope; all
tighten existing sections. Resolved in this revision:

- **A1** §4.4 / §4.5 rewritten to be honest about the code surface —
  `start_sqlite_branch`, `BACKEND_STAGE_READY` inclusion, new
  `SqliteServerConfig` sub-config, new `BackendKind::Sqlite` variant.
  The "no new API" claim retracted.
- **A2** §4.2 notes cross-process subscribe fan-out is a PG-only
  property; multi-ff-server-one-file is intentionally unsupported.
- **A3** §4.3 adds `is_retryable_sqlite_busy` classifier paralleling
  PG's `is_retryable_serialization`; Wave-9 SERIALIZABLE ops wrap
  the classifier.
- **A4** §4.1 pins `required_capabilities` as a normalized junction
  table `ff_execution_capabilities`, not a JSON text-array scan.
  Scanner-supervisor N=1 note added.
- **B1** §4.7 example rewritten against real ff-sdk APIs
  (`FlowFabricAdminClient::new`, `Worker::connect_with`); imports
  complete; `test_config()` replaced with `ServerConfig::sqlite_dev()`
  constructor now committed as a §4.4 scope item.
- **B2** §4.7 split into embedded (cairn-canonical) and HTTP
  examples.
- **B3** `SqliteBackend::new()` embedded-path production-guard parity
  wired in §3.3 and §4.5.
- **B4** `FF_DEV_MODE=1` explicitly orthogonal to existing
  `FF_ENV=development` / `FF_BACKEND_ACCEPT_UNREADY=1` axes;
  documented in §3.3.
- **B5** §9 adds doc-update gates (`CONSUMER_MIGRATION_0.12.md`,
  `DEPLOYMENT.md`, `MIGRATIONS.md`, README env var table, parity
  matrix SQLite column).
- **B6** §4.2 adds the per-process-per-path `SqliteBackend`
  uniqueness invariant with `OnceCell` registry.
- **B7** §4.3 adds the full-Wave-9 coverage justification.
- **C1** §7.3 (parity-drift lint + `.sqlite-skip` sidecar) promoted
  from open question to §4.1 / §9 decision.
- **C2** §1 positioning statement added: SQLite = testing harness;
  Valkey = engine; Postgres = enterprise persistence.
- **C3** §10 adds three missing tax lines (smoke upkeep, docs drift,
  debugging load) + ~80% sizing sentence.
- **C4** §5 non-goal #8 — no Wave-N+ SQLite-only perf/scale work.
- **C5** §9 adds RFC-018 capability-matrix snapshot gate.
- **C6** §10 CI estimate revised from "~2 min" to "3–5 min on
  cold-cache runners."

§7 retained only the two genuine owner forks (SQLite version floor,
publishable-crate posture). §7.3 (parity-drift) moved to decided
scope.

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

### 1.0 Positioning statement (public-facing)

**SQLite is a testing harness; Valkey is the engine; PostgreSQL is
the enterprise persistence layer.** This RFC adds a third backend
scoped explicitly to the testing-harness role. Public docs (README,
comparison pages, consumer migration guides) communicate this
positioning to prevent dilution of FlowFabric's Valkey-native
thesis. Every dev-only marker in this RFC traces back to that
positioning — the scope qualifier is the product promise, not
paperwork.

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
`crates/ff-server/src/server.rs:33` — we already refuse unready
backends at `Server::start_with_metrics`; SQLite joins the list
(`BACKEND_STAGE_READY = &["valkey", "postgres", "sqlite"]`) and
takes a parallel explicit-opt-in gate on top.

**Embedded-path symmetry (B3).** The guard is NOT ff-server-only.
`SqliteBackend::new(path)` — the library entry point used by
no-HTTP embedded consumers per §4.4 — MUST also refuse construction
when `FF_DEV_MODE` is unset, returning a matching `BackendError`
with the same message text. The embedded path is not a production
bypass; every path that produces a `SqliteBackend` handle pays the
guard.

**Single emission point — banner lives on the TYPE (A3).** Both
the HTTP branch (`start_sqlite_branch`) and the embedded path
ultimately construct `SqliteBackend::new`. The banner + `FF_DEV_MODE`
check fire inside `SqliteBackend::new` so every path that yields a
`SqliteBackend` handle gets exactly one banner emission. The HTTP
branch does NOT re-emit the banner; it wraps `SqliteBackend::new`
construction failure into `ServerError::SqliteRequiresDevMode`
(mapping only) and otherwise lets the type-level emission stand.
This prevents duplicate banners under HTTP boot (previously Round-2
text read ambiguously as if both layers emit). See §4.5 for the
enforcement split.

**Relationship to existing dev axes (B4).** FF already has two
dev-leaning env knobs: `FF_ENV=development` and
`FF_BACKEND_ACCEPT_UNREADY=1` (see
`docs/POSTGRES_PARITY_MATRIX.md:249-250`, retired at Stage E4 for
PG but retained as the generic mechanism). `FF_DEV_MODE=1` is
**orthogonal**, not an alias:

- `FF_DEV_MODE=1` is the SQLite-specific production-guard gate.
  It does nothing for `FF_BACKEND=valkey|postgres`.
- `FF_ENV=development` / `FF_BACKEND_ACCEPT_UNREADY=1` remain the
  generic "unready backend stage" override for future backend
  additions before they join `BACKEND_STAGE_READY`.
- SQLite joins `BACKEND_STAGE_READY` at introduction (no stage-E
  ramp), so the generic override is not needed for SQLite; the
  SQLite-specific `FF_DEV_MODE=1` gate does the production
  protection.

Documented in the §9 doc-drop: README env-var table entry calls
out the orthogonal relationship; `docs/dev-harness.md` explains
the separation for operators.

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
1:1 numbered with PG for parity-drift detection. Partitioning is
**dropped** — one non-partitioned table per entity. Correct under
the single-writer / dev-throughput envelope; not correct for PG,
which is why PG partitions. As a corollary, the scanner supervisor
collapses to `N=1` on SQLite — no partition fan-out — since the
flat tables have no partition key to dispatch over.

**Capability-array port (A4).** The PG schema represents
`required_capabilities` as `text[]` with a GIN index for
capability-routing lookups (per RFC-018 / Stage 1 RFC-018). This
does NOT port as a JSON text column + table scan; that would
collapse routing performance on any non-trivial test. The SQLite
port uses a normalized junction table:

```sql
CREATE TABLE ff_execution_capabilities (
    execution_id BLOB NOT NULL,
    capability   TEXT NOT NULL,
    PRIMARY KEY (execution_id, capability)
) WITHOUT ROWID;
CREATE INDEX idx_cap_reverse
    ON ff_execution_capabilities (capability, execution_id);
```

Routing lookups hit the reverse index; insert/update on the parent
execution row drives a delete-then-insert against the junction in
the same transaction. This is the standard SQLite shape for
what PG does with `text[] + GIN`.

**Parity-drift lint — decided in-scope (C1).** CI lints that
`crates/ff-backend-sqlite/migrations/NNNN_*.sql` exists for every
`crates/ff-backend-postgres/migrations/NNNN_*.sql`. A
`.sqlite-skip` sidecar allow-list covers genuinely PG-only
migrations (a partitioning-only admin op, for example); each
skip-list entry MUST cite a tracking issue. Lint wired into
`scripts/lint-migrations.sh` and runs in the same CI job as
`cargo check`. This promotes the Round-1 §7.3 open question to a
decided in-scope deliverable; no more owner fork there.

**`.sqlite-skip` file format (C-trivia-2).** One relative path per
line (pointing at the PG migration that is intentionally skipped);
`#`-prefixed lines are comments; an inline `# issue:NNN` suffix on
a path line is the canonical shape for citing the tracking issue.
Blank lines are ignored. Example:

```
# PG-only partitioning admin migration; no SQLite analogue.
0012_partition_admin_ops.sql  # issue:501

# Full-text search GIN migration - deferred; SQLite FTS5 port TBD.
0013_pg_fts_indexes.sql  # issue:502
```

The lint parses this format directly; no TOML/YAML frame.

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

**Cursor-resume semantic — outbox-inside-tx + broadcast-as-wakeup
(A2).** PG v0.11 achieves late-subscriber cursor-resume (RFC-019)
via outbox tables (`0006_lease_event_outbox.sql`,
`0007_signal_event_outbox.sql`, `0010_operator_event_outbox.sql`)
written INSIDE the operating transaction, plus `pg_notify` as the
wakeup. A consumer that connects after an event committed can still
replay via `SELECT ... FROM outbox WHERE id > :cursor` before
attaching `LISTEN`. The broadcast channel alone cannot provide this
— `tokio::sync::broadcast` has no history beyond its ring buffer,
so a late subscriber would miss committed events.

**SQLite port MUST preserve the same shape:**

- Outbox migrations `0006`, `0007`, `0010` port to SQLite with their
  INSERT paths intact (1:1 numbered per §4.1 parity-drift lint).
  Rows carry the same monotonic cursor column PG uses.
- Write paths (complete/fail/signal/operator-op) INSERT into the
  outbox inside the transaction and then emit on the broadcast
  channel AFTER `tx.commit()` returns. The broadcast emit is the
  WAKEUP only.
- `subscribe_completion` / `subscribe_lease_history` /
  `subscribe_signal_delivery` subscribers cursor-resume from the
  outbox table (SELECT rows after the caller's cursor), then attach
  the broadcast receiver for forward events. Hand-off point is the
  outbox tail at the moment of broadcast subscribe — mirrors the PG
  `LISTEN`-then-`SELECT` handshake.

This satisfies the RFC-019 cursor-resume contract consumers already
depend on without requiring cross-process fan-out (which remains a
permanent non-goal, §5).

**Cross-process subscribe fan-out — intentionally unsupported
(A2).** Cross-process subscribe fan-out is a PG-only property via
`PgListener`. Under SQLite's single-process envelope, a second
ff-server process pointing at the same file will NOT receive
subscribe events originated elsewhere. This is intentional
(§5 non-goal #5) and accepted under the dev envelope. Consumers
needing cross-process semantics pick Valkey or PG; that is the
product purpose of three backends.

**One `SqliteBackend` per file-path per process — invariant
(B6).** Multiple `SqliteBackend::new(path)` handles to the same
file within a process would get separate broadcast channels and
lose cross-handle notifications (handle A writes; handle B's
subscriber never sees it). To prevent this silently:

- Canonicalize the path on construction (`fs::canonicalize`;
  `:memory:` passes through).
- Keep a process-local `OnceCell<Mutex<HashMap<PathBuf,
  Weak<SqliteBackendInner>>>>` registry.
- Second `new(path)` for a live entry returns the existing handle
  (clone of `Arc<SqliteBackendInner>`); entry gone (Weak
  upgraded to `None`) falls through to fresh construction.
- For `:memory:` each call is a separate database by construction
  — the registry entry key includes the per-call UUID from the
  `file:ff-test-<uuid>?mode=memory&cache=shared` URI (§4.6), so
  genuinely-distinct in-memory DBs stay distinct while
  same-URI reuse shares.

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

**Full-Wave-9 surface justification (B7).** SQLite ports the full
Wave-9 surface (operator ops, budget admin, cancel_flow_header,
ack_cancel_member, change_priority, replay_execution, etc.) even
though cairn's `cargo test` primary use case doesn't exercise
every op. Reason: capability-flag parity per RFC-018 must not
lie. If `Supports::CancelFlowHeader` reads `true` on SQLite, the
op must actually work, not panic or return `Unavailable`. The
alternative — per-backend capability carve-outs — fragments the
`Supports` matrix into a "mostly-parity" tier that consumers
cannot reason about cleanly, and opens the door to per-release
drift in what "dev-only" covers. Over-provisioning test coverage
is cheaper than lying about capabilities.

**Retry classifier for SQLite transient errors (A3).** PG has
`is_retryable_serialization` that maps `SerializationFailure` /
`DeadlockDetected` to retry. SQLite's analogue is transient busy
contention on the single-writer lock:

- `SQLITE_BUSY`
- `SQLITE_BUSY_TIMEOUT` (subclass)
- `SQLITE_LOCKED`

Add `is_retryable_sqlite_busy(&sqlx::Error) -> bool` in
`ff-backend-sqlite/src/errors.rs` paralleling PG's classifier.
Wave-9 SERIALIZABLE ops that wrap the classifier: `cancel_flow`,
`cancel_flow_header`, `ack_cancel_member`, `change_priority`,
`replay_execution`, `complete_attempt`, `fail_attempt`,
`deliver_signal`, plus the scanner-supervisor's budget-reconcile
path. Non-retryable kinds (`SQLITE_CORRUPT`, `SQLITE_FULL`, etc.)
surface as hard errors per PG's existing shape — this is the
mechanical mirror of the PG classifier, not a new policy.

### 4.4 Server integration — concrete code surface (A1)

Round-1 Reviewer A correctly flagged that the Round-1 draft
claimed "no new API" while the `start_with_metrics` code at
`crates/ff-server/src/server.rs:299-318` today hard-refuses any
backend not in `BACKEND_STAGE_READY` and then hard-matches
`BackendKind::{Valkey, Postgres}` only. Landing SQLite requires
**concrete, named changes** to server.rs and config.rs, not just
trait-implementation. Enumerated:

1. **`BackendKind::Sqlite`** added to
   `crates/ff-server/src/config.rs:13` (the `#[non_exhaustive]`
   enum). `BackendKind::as_str` returns `"sqlite"`.
2. **`SqliteServerConfig`** added to
   `crates/ff-server/src/config.rs` alongside
   `PostgresServerConfig` / `ValkeyServerConfig`. Fields:
   `path: String` (file path or `:memory:` URI),
   `pool_size: u32` (default 4). `ServerConfig::sqlite:
   SqliteServerConfig` field added.
3. **`ServerConfig::from_env`** (`config.rs:393-407`) extended:
   `FF_BACKEND` accepts `"sqlite"` (alongside `"valkey"` /
   `"postgres"`); `FF_SQLITE_PATH` populates `sqlite.path`;
   `FF_SQLITE_POOL_SIZE` (default 4) populates `sqlite.pool_size`.
4. **`ServerConfig::sqlite_dev()`** constructor added — a
   builder shortcut that returns a pre-wired `ServerConfig` with
   `backend = Sqlite`, `sqlite.path = ":memory:"`, ALL auth axes
   disabled (`api_token = None`, admin-token disabled,
   `waitpoint_hmac_secret` set to a dev-only placeholder that
   refuses to accept tokens from outside the same process — cairn
   tests don't round-trip waitpoint tokens through a network
   surface so this is safe), listen_addr bound to `127.0.0.1:0`
   (OS-picked port), suitable for `Worker::connect_with` embedded
   tests per §4.7. Narrow to dev; no prod path constructs this.
   (C-trivia-1.)
5. **`BACKEND_STAGE_READY`** (`server.rs:33`) extended to
   `&["valkey", "postgres", "sqlite"]`.
6. **`start_sqlite_branch`** added to `server.rs`, parallel to
   the existing `start_postgres_branch` dispatch at
   `server.rs:325-327`. The dispatch chain becomes:
   ```rust
   if matches!(config.backend, BackendKind::Postgres) {
       return Self::start_postgres_branch(config, metrics).await;
   }
   if matches!(config.backend, BackendKind::Sqlite) {
       return Self::start_sqlite_branch(config, metrics).await;
   }
   // ...Valkey default path unchanged...
   ```
   `start_sqlite_branch` performs the §3.3 `FF_DEV_MODE=1`
   production-guard check, emits the warn banner, constructs
   `SqliteBackend::new(&config.sqlite.path)`, skips the
   Valkey-specific scanner / scheduler construction (matching
   the PG branch), and returns a `Server` with the SQLite
   backend wired.
7. **`ServerError::SqliteRequiresDevMode`** added to
   `server.rs` error enum (alongside `BackendNotReady`).
8. **`ServerError` gains `#[non_exhaustive]`** at
   `crates/ff-server/src/server.rs:90` (B1). Ground-truth
   verification at Revision 4: the enum is NOT `#[non_exhaustive]`
   today, so adding `SqliteRequiresDevMode` is a minor breaking
   change for exhaustive-match consumers. The attribute is added
   in the same v0.12.0 PR to absorb this break and all future
   variant additions. §8 rewritten to own this honestly.
9. **`ServerConfig` gains `#[non_exhaustive]`** at
   `crates/ff-server/src/config.rs:103` (B1). Same rationale:
   adding the `sqlite: SqliteServerConfig` field (item 2) breaks
   struct-literal construction, so struct-literal consumers
   already have to migrate; adding `#[non_exhaustive]` in the same
   PR locks in the proper long-term shape.
10. **`ff_sdk::FlowFabricWorker` backend-agnostic refactor** (A1,
    Rev-5 rewrite). Revision 4 framed this as a single cfg-line
    removal. Phase 1a ground-truth verification showed that framing
    is wrong: the `worker` MODULE itself is cfg-gated at
    `crates/ff-sdk/src/lib.rs:93-94`
    (`#[cfg(feature = "valkey-default")] pub mod worker;`); the
    module unconditionally imports `ferriskey::{Client, Value}` at
    `worker.rs:6`; `FlowFabricWorker` embeds a `ferriskey::Client`
    in its struct at `worker.rs:63`; and
    `FlowFabricWorker::connect_with` at `worker.rs:587-596`
    internally calls `Self::connect(config).await?` first (which
    dials Valkey, writes the `ff:worker:{id}:alive` SET-NX
    sentinel, reads `ff:config:partitions`, and wraps a
    `ValkeyBackend`) before swapping in the caller-supplied
    backend. Under cairn's canonical `ff-sdk = { default-features =
    false, features = ["sqlite"] }` posture the module is ABSENT,
    `FlowFabricWorker` does not exist as a symbol, and even if the
    re-export were ungated the type could not compile
    (ferriskey-unresolved) and `connect_with` would still dial
    ferriskey during its preamble. The Revision 4 one-line fix does
    not reach the real blocker.

    **Rev-5 scope (Phase 1a).** Surgical refactor of `worker.rs`
    so `FlowFabricWorker::connect_with` is a genuine backend-agnostic
    entry under any feature set:

    a. **Ungate `pub mod worker;`** at `lib.rs:93-94` — always
       compiled. Same treatment for the `pub use
       worker::FlowFabricWorker;` re-export at `lib.rs:149`.
    b. **Scope ferriskey imports to a cfg-gated inner
       region.** The module-scope `use ferriskey::{Client, Value};`
       at `worker.rs:6` moves behind `#[cfg(feature =
       "valkey-default")]`. Methods that need the concrete
       `ferriskey::Client` stay inside that cfg region.
    c. **Make the `FlowFabricWorker::client` field cfg-gated.**
       The embedded `ferriskey::Client` at `worker.rs:63` becomes
       `#[cfg(feature = "valkey-default")] client: Client,`.
       Under sqlite-only features the struct carries no ferriskey
       state; all backend ops must route through the
       `backend: Arc<dyn EngineBackend>` field.
    d. **Cfg-gate `FlowFabricWorker::connect`** at
       `worker.rs:163-509` behind `#[cfg(feature = "valkey-default")]`.
       Under sqlite-only features `connect()` does not exist as a
       symbol. The rustdoc explicitly calls the method the
       Valkey-only entry point; consumers on non-Valkey backends
       use `connect_with` exclusively.
    e. **Refactor `connect_with`** (`worker.rs:587-596`) so it no
       longer calls `Self::connect(config)`. Instead it directly
       constructs a `FlowFabricWorker` with the supplied `backend`,
       supplied `completion`, a `PartitionConfig` derived from the
       backend (new trait accessor) or defaulted when the backend
       does not expose one, the semaphore built from
       `config.max_concurrent_tasks`, and — under `valkey-default`
       only — an absent `client` field (Option or cfg-gated). No
       PING, no alive-key SET-NX, no Valkey capability
       advertisement, no `ff:config:partitions` read. Existing
       `valkey-default` consumers of `connect_with(config, backend,
       None)` observe the same post-swap state they got before
       (the `connect` preamble constructed a default `ValkeyBackend`
       that was then overwritten; removing it simplifies the path
       and drops one extra Valkey round-trip).
    f. **Cfg-gate all ferriskey-using methods** behind
       `valkey-default`. Ground-truth inventory at Revision 5:
       `claim_next` and its helpers (`block_route`,
       `issue_claim_grant`, `next_lane`, `extract_first_array_string`,
       `is_retryable_claim_error`, `scan_cursor_seed`) are already
       behind `direct-valkey-claim` (which implies
       `valkey-default`) — no change. Additions needed:
       `claim_execution` (`worker.rs:986`), `claim_from_grant`
       (`worker.rs:1201`), `claim_via_server` (`worker.rs:1273`),
       `claim_from_reclaim_grant` (`worker.rs:1337`),
       `claim_resumed_execution` (`worker.rs:1380`),
       `read_execution_context` (`worker.rs:1444`), `deliver_signal`
       (`worker.rs:1487`), and the free function
       `read_partition_config` (`worker.rs:1574`) all gain
       `#[cfg(feature = "valkey-default")]`. Under sqlite-only
       features `FlowFabricWorker` exposes only `connect_with`,
       `backend`, `backend_ref`, `completion_backend`, `config`,
       and `partition_config`; the claim/signal surface is absent
       (a backend-agnostic claim/signal API is deferred to a
       follow-up RFC tracked in the breaking-change disclosure
       below). The `ClaimedTask` surface at `crates/ff-sdk/src/task.rs`
       already gated behind `valkey-default` at `lib.rs:92` stays
       gated — this refactor does not try to make `ClaimedTask`
       backend-agnostic (Stage 1d scope).
    g. **Update `completion_accessor_type_tests`** at
       `worker.rs:1604-1623` — the test is already
       `cfg`-independent but references `FlowFabricWorker`
       from an ungated test module, so it compiles under the
       new layout unchanged. Add a parallel
       `#[cfg(not(feature = "valkey-default"))]` compile-only test
       that asserts `FlowFabricWorker::connect_with` is callable
       with the sqlite-only feature set; this is the mechanical
       anchor for the §9 CI cell.

    **Behavioral impact.** Under `valkey-default` (the default for
    every shipped consumer today) there is zero runtime behavior
    change — the only difference is that `connect_with` no longer
    fires the `connect` preamble's throwaway Valkey round-trips,
    which is a small net improvement not a contract change. Under
    `--no-default-features, features = ["sqlite"]` the `worker`
    module compiles, `FlowFabricWorker::connect_with(config,
    backend, None)` works, and the cairn-canonical §4.7.1 example
    runs without any Valkey reachability.

    **Public API shape after Rev-5 Phase 1a:**
    - `FlowFabricWorker` — always addressable (module + re-export
      ungated).
    - `FlowFabricWorker::connect_with(config, backend, completion)`
      — always available; standalone, no `connect` preamble.
    - `FlowFabricWorker::backend` / `backend_ref` /
      `completion_backend` / `config` / `partition_config` —
      always available.
    - `FlowFabricWorker::connect` — `valkey-default`-gated;
      ABSENT under sqlite-only features. Documented as the
      Valkey-specific convenience entry that bundles
      `ValkeyBackend` construction.
    - Claim + signal methods (`claim_next`, `claim_from_grant`,
      `claim_via_server`, `claim_from_reclaim_grant`,
      `deliver_signal`) — `valkey-default`-gated. Under
      sqlite-only features the returned worker can hold a backend
      and hand out `backend()` references but cannot drive the
      claim/signal loop (§4.7.1 primary cairn path drives the
      backend directly via `EngineBackend` trait methods; a
      backend-agnostic SDK worker-loop is deferred to a future
      RFC tracked in the §8 breaking-change disclosure).

    **Net diff estimate.** ~50–100 lines changed in `worker.rs`
    (cfg attributes on module-scope imports, struct field,
    `connect`, 7 additional methods + 1 free function;
    `connect_with` body rewritten to ~20 lines of direct
    construction replacing the 10-line `Self::connect` preamble).
    1 line removed at `lib.rs:93` (module gate); 1 line
    removed at `lib.rs:149` (re-export gate). No new public
    types, no trait-signature changes. Tests: one new
    `#[cfg(not(feature = "valkey-default"))]` compile-only
    assertion in `worker.rs` matching the existing pattern at
    `worker.rs:1604-1623`.

**Embedded, no-HTTP consumers** continue to use the existing
`Server::start_with_backend` (`server.rs:677`) by constructing
`SqliteBackend::new(path).await?` and wrapping in `Arc<dyn
EngineBackend>`. That API signature is unchanged. `Worker::connect_with`
(`ff-sdk/src/worker.rs:587`) accepts the same `Arc<dyn
EngineBackend>` shape and is the cairn-canonical embedded path.

**Public API stability commitments taking effect at v0.12.0:**

- `SqliteBackend::new(path: &str) -> Result<Self, BackendError>`
- `BackendKind::Sqlite`
- `ServerConfig::sqlite: SqliteServerConfig`
- `ServerConfig::sqlite_dev() -> Self`
- `ServerError::SqliteRequiresDevMode` (variant)
- `ServerError` and `ServerConfig` gain `#[non_exhaustive]` (B1)
- `ff_sdk::FlowFabricWorker` becomes addressable under
  `default-features = false, features = ["sqlite"]` via the
  Rev-5 Phase 1a `worker.rs` surgery (A1): module + re-export
  cfg-gates removed, `connect_with` refactored to a standalone
  backend-agnostic constructor, ferriskey-using methods
  (`connect`, `claim_next`, `claim_from_grant`,
  `claim_via_server`, `claim_from_reclaim_grant`,
  `deliver_signal`, and internal helpers) cfg-gated behind
  `valkey-default` (§4.4 item 10)
- `FF_BACKEND=sqlite`, `FF_SQLITE_PATH`, `FF_SQLITE_POOL_SIZE`,
  `FF_DEV_MODE`

Option X (a hypothetical `Server::start_embedded` with no HTTP
bind) is not needed — `start_with_backend` already covers
library-level use; the earlier "Option Y" framing was correct in
outcome but wrong to claim "no new API." The new API is the four
items above; the library-level boot path itself is unchanged.

### 4.5 Production-guard mechanism

Per §3.3. Emission lives on the TYPE (§3.3 A3); the HTTP branch
maps errors and does not double-emit.

**Embedded path — `SqliteBackend::new`** (library entrypoint in
`crates/ff-backend-sqlite/src/lib.rs`, the SINGLE enforcement +
emission point): checks
`std::env::var("FF_DEV_MODE").as_deref() == Ok("1")` at the top of
the function. Absent → returns a new `BackendError::RequiresDevMode`
variant (parallel shape to `ServerError::SqliteRequiresDevMode`;
adds a new variant on the existing `#[non_exhaustive]`
`BackendError` enum at `crates/ff-core/src/engine_error.rs:488`.
Does NOT extend `BackendErrorKind`, which is scoped to Valkey
transport classification per its existing rustdoc). Present →
emits the warn banner on construction.

**Committed embedded-path error text (B2).** The
`BackendError::RequiresDevMode` message renders as:

```
SqliteBackend requires FF_DEV_MODE=1 to activate. SQLite is
dev-only; see https://github.com/avifenesh/FlowFabric/blob/main/docs/dev-harness.md
for details.
```

This parallels the §3.3 HTTP-path text (both include the
dev-harness.md URL) and gives embedded consumers the same
actionable signal. Kept in sync with §3.3 by the smoke-script
assertion in `scripts/smoke-sqlite.sh` (§9).

**HTTP path — `Server::start_sqlite_branch`** (new, §4.4 item 6
in `crates/ff-server/src/server.rs`): delegates to
`SqliteBackend::new`. When that returns
`BackendError::RequiresDevMode`, the branch maps the error into
`ServerError::SqliteRequiresDevMode` (with the §3.3 text) and
returns it to the caller. The branch does NOT re-check
`FF_DEV_MODE` itself and does NOT re-emit the banner — the type
already did both. This keeps the guard single-sourced and prevents
two-banner output under HTTP boot.

**Rationale for TYPE placement.** cairn and FF-internal embedded
tests (§4.7 primary example) instantiate SQLite without ever
touching ff-server; the guard cannot live only at the ff-server
layer or it is trivially bypassed by
`Worker::connect_with(…, Arc::new(SqliteBackend::new(path)))`.
The guard is on the TYPE, not the server.

Banner emits on every `SqliteBackend::new` success so log
aggregators can alert if a SQLite backend ever reaches an
environment it shouldn't.

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

Two supported shapes. Cairn-canonical = **embedded (no HTTP)**.
The HTTP shape is for consumer harnesses that want the full
ff-server boundary exercised.

#### 4.7.1 Embedded path — primary, cairn-canonical

No HTTP listener, no bind port, no `reqwest` dependency — the
`FlowFabricWorker` directly wraps the SQLite backend trait object. This
is the shape cairn's `cargo test` uses.

> **Compile-surface note (A1, Rev-5).** §4.4 item 10 commits both
> the `pub mod worker;` module gate at
> `crates/ff-sdk/src/lib.rs:93` and the
> `pub use worker::FlowFabricWorker;` re-export gate at
> `crates/ff-sdk/src/lib.rs:149` to be ungated, plus the
> `worker.rs` surgery that cfg-gates `FlowFabricWorker::connect`
> and the Valkey-specific claim/signal methods behind
> `valkey-default`. The example below assumes that Phase 1a change
> is landed; without it the `use ff_sdk::FlowFabricWorker` line
> fails to resolve under the sqlite-only posture (module absent).
>
> **Post-surgery `connect_with` behavior (Rev-5).**
> `FlowFabricWorker::connect_with(config, backend, None)` directly
> constructs the worker with the supplied `backend` — no
> `Self::connect(config)` preamble runs, no ferriskey client is
> dialed, no PING is issued, no `ff:worker:{id}:alive` SET-NX
> sentinel is written, no `ff:config:partitions` HGETALL is
> performed. Consumer code under
> `ff-sdk = { default-features = false, features = ["sqlite"] }`
> can safely call `connect_with` from any context without
> unsetting `FF_HOST` / `FF_PORT` / `FF_CONNECTION_URL`
> environment variables — the code path that read them is
> compiled out. The Revision 4 `.cargo/config.toml [env]`
> stray-env mitigation is obsolete once Phase 1a ships and is NOT
> required for cairn's canonical posture. (`FF_DEV_MODE=1` is
> still required — §4.5 enforcement lives on the
> `SqliteBackend::new` type and applies regardless of worker
> wiring.)
>
> **What the worker can and cannot do under sqlite-only features.**
> Post-Rev-5 the worker exposes `connect_with`, `backend` /
> `backend_ref`, `completion_backend`, `config`, and
> `partition_config` under the sqlite-only feature set. The
> claim/signal surface (`claim_next`, `claim_from_grant`,
> `claim_via_server`, `claim_from_reclaim_grant`, `deliver_signal`)
> is `valkey-default`-gated and ABSENT under sqlite-only features
> — a backend-agnostic SDK claim/signal loop is deferred to a
> future RFC (§8 breaking-change disclosure). Cairn's canonical
> tests drive the backend directly via `EngineBackend` /
> `CompletionBackend` trait methods on the returned `backend()` /
> `completion_backend()` handles, which is the intended Phase 1a
> shape and matches how `ff-server`'s own Postgres branch
> exercises the trait surface today.

```rust
// cairn-fabric/tests/integration_sqlite.rs
use std::sync::Arc;
use ff_backend_sqlite::SqliteBackend;
use ff_sdk::{FlowFabricWorker, WorkerConfig};

#[tokio::test]
async fn end_to_end_op_roundtrip_on_sqlite() {
    // FF_DEV_MODE=1 must be set by the harness (e.g. in the
    // `[env]` block of .cargo/config.toml, or the shell running
    // `cargo test`). `SqliteBackend::new` refuses without it per
    // §4.5.
    std::env::set_var("FF_DEV_MODE", "1");

    let db_uri = format!(
        "file:cairn-test-{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4(),
    );
    let backend = Arc::new(
        SqliteBackend::new(&db_uri).await.expect("sqlite init"),
    );

    // `FlowFabricWorker::connect_with` accepts any `Arc<dyn EngineBackend>`
    // (crates/ff-sdk/src/worker.rs:587). No ff-server needed.
    let config = WorkerConfig::builder()
        .lanes(vec!["default".into()])
        .build()
        .expect("worker config");
    let worker = FlowFabricWorker::connect_with(config, backend, None)
        .await
        .expect("worker connect");

    // ... cairn's existing test logic against `worker`, unchanged ...
}
```

#### 4.7.2 HTTP path — secondary, operator / consumer smoke

For consumer harnesses that want to drive the full REST surface
(the `published-smoke.sh` shape, and the consumer-migration doc
example):

```rust
// examples/ff-dev or consumer smoke test
use ff_backend_sqlite::SqliteBackend;
use ff_sdk::admin::FlowFabricAdminClient;
use ff_server::config::ServerConfig;
use ff_server::server::Server;
use std::sync::Arc;

#[tokio::test]
async fn http_roundtrip_on_sqlite() {
    std::env::set_var("FF_DEV_MODE", "1");

    // `ServerConfig::sqlite_dev()` is the §4.4 item 4 builder —
    // pre-wires backend=Sqlite, sqlite.path=":memory:",
    // listen_addr=127.0.0.1:0, auth disabled.
    let config = ServerConfig::sqlite_dev();

    let backend = Arc::new(
        SqliteBackend::new(&config.sqlite.path).await.unwrap(),
    );
    let metrics = Arc::new(ff_observability::Metrics::new());
    let server = Server::start_with_backend(config, backend, metrics)
        .await
        .expect("server start");

    // `FlowFabricAdminClient::new` is the real ff-sdk API
    // (crates/ff-sdk/src/admin.rs:50). No `Client::connect` —
    // that doesn't exist.
    let admin = FlowFabricAdminClient::new(
        format!("http://{}", server.listen_addr()),
    ).expect("admin client");

    // ... smoke logic against `admin` ...
}
```

Before (both shapes, pre-RFC-023): `docker compose up postgres`
+ `FF_POSTGRES_URL` + per-test schema isolation. After: 20–35
lines and `cargo test`.

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
8. **NOT Wave-N+ SQLite-only performance or scale work.** Any
   future Wave that improves SQLite performance in isolation of
   its PG equivalent is permanently out of scope. SQLite tracks
   PG v0.11+ parity; it does not receive dedicated perf or scale
   RFCs. If SQLite acquires a performance gap against PG, the
   resolution is either (a) accept the gap (dev envelope) or (b)
   port the PG fix; never (c) a SQLite-specific perf RFC.

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

Two remain after Round-2. The Round-1 §7.3 (parity-drift lint)
is promoted to §4.1 as in-scope.

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

*(Round-1 §7.3 parity-drift lint was promoted to §4.1 as a
decided in-scope deliverable.)*

---

## 8. Migration impact (consumer-facing)

**Mostly additive, with two minor breaking changes for exhaustive
consumers (B1).** Runtime behavior for `FF_BACKEND=valkey` and
`FF_BACKEND=postgres` is unchanged; the breaks are compile-time
shape adjustments on two ff-server types that were not marked
`#[non_exhaustive]`.

**Unchanged:**

- `FF_BACKEND=valkey` (default) and `FF_BACKEND=postgres` runtime
  behavior identical to v0.11.
- No schema change in Postgres or Valkey backends.
- No trait change in `ff-core::engine_backend`.
- No capability-matrix change for the two existing backends.

**Breaking at v0.12.0 (committed disposition: Reviewer B option
(a) — own the break and mark both types `#[non_exhaustive]` in the
same PR):**

1. **`ServerError` gains `SqliteRequiresDevMode` variant + becomes
   `#[non_exhaustive]`.** Ground-truth at Revision 4:
   `crates/ff-server/src/server.rs:90` enum has NO
   `#[non_exhaustive]` today. Consumers writing exhaustive `match`
   arms on `ServerError` will fail to compile and must add a
   wildcard arm or the new variant. Pre-1.0 project posture makes
   this the right moment to seal the enum against future additions.
2. **`ServerConfig` gains `sqlite: SqliteServerConfig` field +
   becomes `#[non_exhaustive]`.** Ground-truth at Revision 4:
   `crates/ff-server/src/config.rs:103` struct has NO
   `#[non_exhaustive]` today. Consumers constructing
   `ServerConfig { ... }` via struct literal must migrate to the
   `from_env` path, the `sqlite_dev()` constructor (§4.4 item 4),
   or `..Default::default()` syntax. Existing `ServerConfig::from_env`
   callers are unaffected; the field populates with defaults under
   non-sqlite `FF_BACKEND` values.

3. **`ff_sdk::worker` module cfg-gating shift (Rev-5, A1).** The
   `ff_sdk::worker` module moves from `valkey-default`-gated
   (`crates/ff-sdk/src/lib.rs:93`) to always-compiled.
   `FlowFabricWorker::connect` becomes
   `#[cfg(feature = "valkey-default")]` and is ABSENT under
   `--no-default-features`; likewise `claim_next`,
   `claim_from_grant`, `claim_via_server`,
   `claim_from_reclaim_grant`, and `deliver_signal`. Consumers who
   built with `--no-default-features` AND directly called these
   methods (no such consumer exists on crates.io today per a
   2026-04-26 scan; this is a forward-looking commitment) must
   either enable the `valkey-default` feature to restore the
   Valkey-bundled convenience path or switch to the backend-agnostic
   `connect_with(config, backend, completion)` entry plus direct
   `EngineBackend` trait calls through `backend()` for the
   claim/signal surface. A backend-agnostic SDK worker-loop is
   deferred to a future RFC (tracked as a §9 follow-up issue at
   v0.12.0 tag time); until that RFC lands, consumers using the
   SDK on non-Valkey backends drive the claim/signal ops through
   the trait surface directly. This is a pre-1.0 minor break,
   consistent with the existing RFC-023 §8 posture.

Both ff-server breaks plus the ff-sdk shift are noted in
`CHANGELOG.md [Unreleased] ### Changed` per the §9 release-gate
discipline and called out in `docs/CONSUMER_MIGRATION_0.12.md`.
The alternative dispositions considered:

- **Option (b) — route SQLite refusal through existing
  `ServerError::InvalidInput` / `OperationFailed`.** Avoids the
  `ServerError` variant add, but still requires the `ServerConfig`
  field add (hence still breaking struct-literal consumers).
  Delivers a less-classifiable error shape for a short-term
  `#[non_exhaustive]` deferral. Rejected.
- **Option (c) — accept both as minor breaking, do NOT add
  `#[non_exhaustive]` now.** Saves one line-change today; pays
  the same consumer break every time a future RFC adds another
  variant/field. Rejected as non-ideal long-term posture.

Consumer opt-in path (cairn example):

1. `cargo add ff-backend-sqlite` (or enable `flowfabric/sqlite`
   feature, per §7.2).
2. Set `FF_BACKEND=sqlite` + `FF_DEV_MODE=1` +
   `FF_SQLITE_PATH=:memory:` in the test harness env.
3. If constructing `ServerConfig` via struct literal, switch to
   `ServerConfig::from_env()` or `ServerConfig::sqlite_dev()` or
   use `..Default::default()` spread.
4. If exhaustively matching `ServerError`, add wildcard arm.
5. Replace `docker compose up postgres` preamble with nothing.
6. `cargo test` — passes.

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
      **Content spec (B3):** canonical `FF_DEV_MODE=1` setup is
      the `.cargo/config.toml [env]` block (reliable across
      parallel `cargo test` invocations, survives workspace
      members); `std::env::set_var` in test bodies is shown only
      as a fallback with an explicit parallel-test-safety caveat
      (the env is process-global and racy across threads). The
      doc also lists stray-env cleanup (`FF_HOST` / `FF_PORT` /
      `FF_CONNECTION_URL`) per the §4.7.1 footnote.
- [ ] `examples/ff-dev/` example compiles + runs + documents
      "try it in 60 seconds."
- [ ] RELEASING.md updated if §7.2 lands as Option A (new
      publishable crate).
- [ ] `release.yml` + `release.toml` updated if new publishable
      crate.
- [ ] **New `docs/CONSUMER_MIGRATION_0.12.md`** following the
      v0.10 / v0.11 pattern — "how cairn migrates a test file
      from Docker-PG to SQLite-dev in ~20 lines." (B5)
- [ ] **`docs/DEPLOYMENT.md` updated** with an explicit
      "SQLite is NOT a deployment target" section citing
      `FF_DEV_MODE=1` rationale and the §1.0 positioning
      statement. (B5)
- [ ] **`docs/MIGRATIONS.md` updated** with env var row for
      `FF_BACKEND=sqlite`, `FF_SQLITE_PATH`, `FF_SQLITE_POOL_SIZE`,
      `FF_DEV_MODE`. (B5)
- [ ] **Root `README.md` env var table** (`README.md:140-148`)
      extended with the four new env vars (`FF_BACKEND=sqlite`
      accepted value, `FF_SQLITE_PATH`, `FF_SQLITE_POOL_SIZE`,
      `FF_DEV_MODE`). (B5)
- [ ] **Parity matrix**: `docs/POSTGRES_PARITY_MATRIX.md` gains a
      SQLite column (filename kept as-is to avoid URL churn in
      external links; renaming to `BACKEND_PARITY_MATRIX.md`
      flagged in §7 if owner wants the rename). (B5)
- [ ] **RFC-018 capability-matrix snapshot test on SQLite**
      passes — every `Supports` flag matches PG v0.11's, no gaps.
      This is the mechanical enforcer of §4.3's parity
      commitment. (C5)
- [ ] **Parity-drift migration lint** (§4.1) green: every PG
      migration has a SQLite sibling or a `.sqlite-skip` entry
      with tracking issue.
- [ ] **ff-sdk no-default-features compile gate (Rev-5, §4.4 item
      10).** A CI cell runs
      `cargo check -p ff-sdk --no-default-features --features sqlite`
      and passes. Catches regressions where a future PR adds a
      ferriskey call outside the `valkey-default` cfg gate on
      `FlowFabricWorker` or a sibling module. Lives alongside the
      existing `cargo check -p ff-core --no-default-features
      --features core` cell in `.github/workflows/matrix.yml`
      (currently at line 200-201 of that file); adds ~15 seconds
      wall-clock on cold-cache runners.
- [ ] **CHANGELOG entries.** `CHANGELOG.md` `[Unreleased]` entries
      under `### Added` for the four new public APIs
      (`BackendKind::Sqlite`, `SqliteServerConfig`,
      `ServerConfig::sqlite_dev()`, `SqliteBackend::new`), plus the
      new `BackendError::RequiresDevMode` variant per §4.5.
      Consistent with CLAUDE.md §5 release-gate discipline.

Per [feedback_release_publish_list_drift](../../memory/feedback_release_publish_list_drift.md),
release-config files (release.yml, release.toml, RELEASING.md)
are updated in the same PR that introduces the crate.

---

## 10. Maintenance tax commitment

Honest enumeration:

- **Schema-changing RFCs port 1 more migration per PR.** §4.1
  parity lint enforces.
- **Trait-surface RFCs add a third backend impl per PR.** Same
  discipline as ff-backend-valkey; SQLite is row 3.
- **CI matrix.** One new cell `cargo test --workspace --features
  sqlite`. Estimate **3–5 minutes** on cold-cache CI runners —
  the bundled sqlx compile dominates the wall clock, not the
  tests themselves. Warm-cache runs are sub-minute. (C6)
- **Parity-drift detection.** §4.1 migration lint + RFC-018
  capability-matrix snapshot test (§9). Two mechanical
  enforcers; both in CI before merge.
- **Bundled SQLite binary size.** sqlx bundled build adds
  ~500 KB; feature-gated, non-opt-in consumers don't pay.
- **Postgres-RFC cognitive load.** Third-backend consideration
  on every PG PR; valkey already precedents this, RFC-017
  through RFC-020 landed without blockage.
- **Smoke-script upkeep.** (C3) `scripts/smoke-sqlite.sh` +
  the SQLite scenario in `scripts/published-smoke.sh` are
  ongoing release-gate weight. Every env-var rename, every
  `ServerConfig::sqlite_dev` shape change, and every
  `SqliteBackend::new` signature change drives a smoke-script
  edit. Budget this as ~1 smoke-script touch per 2 releases
  steady state.
- **Docs-drift risk.** (C3) `docs/dev-harness.md` + README
  "60 seconds" section + §4.7 examples + `CONSUMER_MIGRATION_*`
  + `DEPLOYMENT.md` + `MIGRATIONS.md` — six docs paths all
  carrying SQLite references. Any env-var or config shape
  change drives a sync-or-drift audit across them. RFC-023
  lands the baseline; future RFCs that touch backend config
  must sweep all six.
- **Debugging load.** (C3) Contributors hit SQLite-only
  dialect-gap, WAL-mode, and single-writer serialization
  quirks that don't exist on PG or Valkey (e.g. `SQLITE_BUSY`
  storms under parallel test load, WAL files surviving a
  crash and confusing re-open). Triage time comes out of
  maintainer bandwidth, not automation.
- **`ff_sdk::worker` cfg-regression review (Rev-5).** Every
  future PR that adds a Valkey-specific worker method, adds a
  ferriskey API call, or introduces a new ferriskey-dependent
  helper on the `FlowFabricWorker` surface must be reviewed for
  `#[cfg(feature = "valkey-default")]` gate correctness. The §9
  `cargo check -p ff-sdk --no-default-features --features sqlite`
  CI cell catches regressions mechanically (adds ~15 seconds to
  the matrix on cold cache) but the human review tax is that
  every reviewer on an ff-sdk worker change checks the gate
  discipline, the same way every PG PR already checks the
  third-backend-impl discipline.

**Tax sizing.** Maintenance tax is estimated at **~80% of the
full-parity RFC-022 shape's cost.** The 20% saved is
production-operations surface that dev-only deliberately skips:
no prod SQLite bug reports, no perf tuning under real load, no
HA debugging, no cluster/replication operator guides. Owner has
accepted the 80% figure per the "SQLite for testing" directive.

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
