# Postgres operator guide

Tuning notes for running FlowFabric's Postgres backend
(`ff-backend-postgres`) under production load.

## `max_locks_per_transaction`

**Recommended:** `512` (minimum `256`).
**Postgres default:** `64`.

### Why

The RFC-v0.7 Postgres schema hash-partitions every hot table into
256 child partitions (`ff_exec_core_pNN`, `ff_edge_group_pNN`, etc.).
A single transaction that touches rows across many partitions —
common in the dispatch cascade, flow cancellation, and reconciler
scans — acquires one lock per partition it touches, plus its
index partitions. A fanout transaction can easily demand well
beyond 64 locks.

Under modest concurrent load the shared lock table fills and
transactions abort with:

```
ERROR: out of shared memory
HINT:  You might need to increase "max_locks_per_transaction"
```

Wave 7c's Phase 0 benchmark hit this at 16 workers × 10k tasks
against a Postgres 16 with default settings. Raising the setting
to `512` cleared the failure without further tuning.

### How

In `postgresql.conf`:

```
max_locks_per_transaction = 512
```

Requires a server restart. On managed providers (RDS, Cloud SQL,
etc.) this is a parameter-group setting followed by a reboot.

### Boot-time warning

`PostgresBackend::connect` runs `SHOW max_locks_per_transaction`
at dial time and logs a `tracing::warn` when the value is below
`256`. The warning is informational — the backend does not refuse
to start, since operators may have platform reasons to run with a
non-default-but-still-below-256 value. The warning surfaces as:

```
postgres max_locks_per_transaction=64 is below the recommended minimum (256);
partition-heavy workloads may hit 'out of shared memory' under concurrent load.
```

If the probe itself fails (`SHOW` denied on exotic deploys) the
error is logged at `debug` and swallowed.
