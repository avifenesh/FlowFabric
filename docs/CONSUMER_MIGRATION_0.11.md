# Consumer migration — v0.10 → v0.11

**Scope.** v0.11 ships **RFC-020 Wave 9**: the 12 remaining Postgres
`Unavailable` trait methods flip to `impl` atomically. No Rust API
changes, no wire-format breaks — the surface is unchanged. Consumers
see new Postgres capabilities, new Postgres migrations to run, and a
new `ff_operator_event` LISTEN/NOTIFY channel on the Postgres backend.

A full per-change listing lives in the `[0.11.0]` section of
`CHANGELOG.md`; this doc focuses on the operator + consumer code
checklist for adopting the new capabilities.

## What shipped

### 1. Postgres backend — 12 capability flips (#RFC-020, 5 PRs)

`PostgresBackend::capabilities()` now reports `true` for:

- Operator control: `cancel_execution`, `change_priority`,
  `replay_execution`, `revoke_lease`
- Read model: `read_execution_info`, `read_execution_state`,
  `get_execution_result`
- Budget / quota: `budget_admin` (covers `create_budget`,
  `reset_budget`, `get_budget_status`, `report_usage_admin`),
  `quota_admin` (covers `create_quota_policy`)
- Waitpoints: `list_pending_waitpoints`
- Flow cancel split: `cancel_flow_header`, `ack_cancel_member`

Previously every one of these returned `EngineError::Unavailable { op }`
on Postgres → HTTP 503. From v0.11.0, Postgres serves them concretely
with behaviour matching Valkey. Call sites that greyed-out operator UI
actions based on `caps.supports.<field>` now render them enabled once
the consumer upgrades.

`subscribe_instance_tags` remains `false` on both backends per #311
(speculative demand, served by `list_executions` +
`ScannerFilter::with_instance_tag`).

Design record: [`rfcs/RFC-020-postgres-wave-9.md`](../rfcs/RFC-020-postgres-wave-9.md).

### 2. Postgres migrations 0010 – 0014 (required before upgrade)

Five additive migrations. All forward-only; no rollback. Run
`sqlx migrate run` (or equivalent) against every Postgres deployment
backing a v0.11.0 consumer **before** the new binary serves traffic:

| # | Adds | Purpose |
|---|---|---|
| 0010 | `ff_operator_event` outbox + trigger + `NOTIFY ff_operator_event` | New LISTEN/NOTIFY channel for operator-control events (`priority_changed` / `replayed` / `flow_cancel_requested`); preserves the RFC-019 `ff_signal_event` subscriber contract. |
| 0011 | `state`, `required_signal_names`, `activated_at_ms` columns on `ff_waitpoint_pending` | Required to serve the real `PendingWaitpointInfo` contract. Producer-side `suspend_ops` writes these on insert + activation (shipped in the same PR). `waitpoint_key` was already present from migration 0004 (no new column). |
| 0012 | `ff_quota_policy` + `ff_quota_window` + `ff_quota_admitted` (256-way HASH-partitioned on `partition_key`) | Quota-policy schema backing `create_quota_policy`. Concurrency is a column on `ff_quota_policy`, not a dedicated counter table. |
| 0013 | `ff_budget_policy` additive columns: `next_reset_at_ms` + 4 breach-tracking + 3 definitional | Budget scheduling + in-line breach-counter bookkeeping. Enables the new Postgres-native `BudgetResetReconciler`. |
| 0014 | `ff_cancel_backlog` table | Per-member cancel backlog driving `cancel_flow_header` + `ack_cancel_member` + the cancel-backlog reconciler. |

If the consumer deployment auto-runs `apply_migrations` at boot
(default for `ff-server`), nothing extra to do. Operators running
migrations out-of-band must apply 0010–0014 in order before rolling
v0.11.0 binaries.

### 3. New LISTEN channel on Postgres (`ff_operator_event`)

Operators subscribing to Postgres LISTEN/NOTIFY channels for ops
dashboards should add `ff_operator_event` alongside the existing
`ff_lease_event` / `ff_signal_event` / `ff_completion_event` channels.
The trait-level subscription APIs (`subscribe_*`) do not expose this
channel directly — it's used internally by the Postgres impls of
`change_priority` / `replay_execution` / `cancel_flow_header` to emit
audit events where Valkey would XADD. Consumers that only use the
public trait surface can ignore it.

## Non-changes

- No Rust API surface change (no new trait methods, no signature
  changes, no renames).
- No wire-format change on the HTTP / JSON surface.
- Valkey backend behaviour unchanged (RFC-020 §5.2 mandate).
- No new features added to unrelated subsystems.

## Upgrade checklist

- [ ] `cargo update -p flowfabric` (or the ff-* sub-crates) to 0.11.0.
- [ ] **Postgres only:** run `sqlx migrate run` against every deployment
      before serving v0.11.0 traffic. Migrations 0010–0014 are forward-
      only.
- [ ] If UI grey-rendering off `capabilities().supports.<field>`:
      audit the 12 newly-`true` flags and remove any "this backend
      does not support X" banners that are now stale on Postgres.
- [ ] Optional: if you subscribe to Postgres LISTEN channels directly,
      add `ff_operator_event` to your subscriber set.
- [ ] Run your integration smoke. File issues against this repo for
      any gap you hit; the Wave 9 design record is in `rfcs/RFC-020`.
