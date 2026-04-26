# v011-wave9-postgres — Postgres Wave 9 headline demo

Exercises every one of the six Wave-9 method groups that flipped from
`Unavailable` to `impl` on the Postgres backend in v0.11.0 (RFC-020
Rev 7).

## What it does

The demo runs as a miniature **multi-tenant operator dashboard**:

1. Provisions two tenants (A + B) with `create_budget` + one shared
   quota policy via `create_quota_policy`.
2. Reports usage for tenant A with `report_usage_admin` and reads
   back `get_budget_status`.
3. Seeds three runnable executions and one failed execution directly
   (admin path — no workers in the demo).
4. Drives the full operator-control surface against the seeded execs:
   - `change_priority` — bump a hot exec.
   - `cancel_execution` — cancel a stuck exec.
   - `replay_execution` — revive a failed exec.
   - `list_pending_waitpoints` — scoped waitpoint listing.
   - `cancel_flow_header` — kill-switch a whole flow.
5. Audits every touched exec through `read_execution_info`.

## Why it's shaped this way

The Wave 9 surface is **admin / operator control**, not the
create/dispatch/claim hot path. Driving a real execution end-to-end
through a worker is what the `token-budget` and `llm-race` examples
are for. Here we seed minimal `ff_exec_core` + `ff_attempt` rows
directly so every Wave-9 trait method is callable in a 200-line demo
against a fresh Postgres.

## Prereqs

- Postgres 16+ reachable at `FF_PG_TEST_URL`
  (e.g. `postgres://postgres:postgres@localhost:5432/ff_v011_demo`).
- A clean database, or one already carrying the v0.11 schema. The
  example runs `apply_migrations` at boot, so a brand-new empty db
  works out of the box.

## Run

```bash
FF_PG_TEST_URL=postgres://postgres:postgres@localhost:5432/ff_v011_demo \
    cargo run -p v011-wave9-postgres-example
```

Expected log shape (abridged):

```
dialed Postgres backend
Wave 9 capability flags — all true on v0.11+
tenant-a report_usage_admin
tenant-a budget status
operator change_priority ok
operator cancel_execution ok
ack_cancel_member surface live
operator replay_execution ok
list_pending_waitpoints ok
cancel_flow_header ok
read_execution_info audit  (x5)
demo complete — all 6 Wave-9 method groups exercised on Postgres v0.11.0
```

## Related

- RFC-020 Rev 7 — [`rfcs/RFC-020-postgres-wave-9.md`](../../rfcs/RFC-020-postgres-wave-9.md)
- Consumer migration — [`docs/CONSUMER_MIGRATION_0.11.md`](../../docs/CONSUMER_MIGRATION_0.11.md)
- Parity matrix — [`docs/POSTGRES_PARITY_MATRIX.md`](../../docs/POSTGRES_PARITY_MATRIX.md)
