-- no-transaction
-- RFC-020 Wave 9 Standalone-1 — `ff_budget_policy` additive columns
-- for breach-counter + scope + reset-scheduling surfaces (Revision 6
-- §4.4.1 + §4.4.5).
--
-- The real `BudgetStatus` contract
-- (`crates/ff-core/src/contracts/mod.rs:1413-1430`) has 9 fields not
-- present on the pre-Wave-9 `ff_budget_policy` schema shipped by
-- `0002_budget.sql`. This migration adds:
--
--   * `scope_type`         — e.g. "flow", "namespace" (from CreateBudgetArgs).
--   * `scope_id`           — matching scope reference.
--   * `enforcement_mode`   — "strict" | "warn".
--   * `breach_count`       — hard-breach counter (HINCRBY twin).
--   * `soft_breach_count`  — soft-breach counter (HINCRBY twin).
--   * `last_breach_at_ms`  — last HardBreach wall time.
--   * `last_breach_dim`    — dimension name of last HardBreach.
--   * `next_reset_at_ms`   — scheduler pointer for the `budget_reset`
--                            reconciler (NULL = no auto-reset).
--
-- `created_at_ms` is NOT added — it already exists on
-- `ff_budget_policy` from migration 0002. The `BudgetStatus.created_at`
-- field reads that pre-existing column.
--
-- Plus: a partial index `ff_budget_policy_reset_due_idx` that backs
-- the `budget_reset` reconciler scan (RFC §4.4.3). Partial because
-- most budgets have `next_reset_at_ms = NULL` (no scheduled reset);
-- non-scheduled rows must not bloat the index.
--
-- Existing rows (pre-0013) get `scope_type=''`, `breach_count=0`,
-- `next_reset_at_ms=NULL`. Wave 9's Standalone-1 impl populates the
-- columns on `create_budget` writes post-0013; pre-0013-inserted
-- rows return `scope_type=''` etc. in `get_budget_status` — degraded
-- but non-breaking.
--
-- Postgres 16 floor (`.github/workflows/matrix.yml` pins
-- `postgres:16-alpine`): `ALTER TABLE … ADD COLUMN` with a defaulted
-- NOT NULL stores the default in catalog metadata and runs
-- O(1)-metadata-only (no table rewrite). Partition-child rewrite is
-- not needed.
--
-- `backward_compatible = true` — all adds are nullable-or-defaulted;
-- pre-0013 binaries ignore the columns and stay correct.

ALTER TABLE ff_budget_policy
    ADD COLUMN scope_type          text   NOT NULL DEFAULT '',
    ADD COLUMN scope_id            text   NOT NULL DEFAULT '',
    ADD COLUMN enforcement_mode    text   NOT NULL DEFAULT '',
    ADD COLUMN breach_count        bigint NOT NULL DEFAULT 0,
    ADD COLUMN soft_breach_count   bigint NOT NULL DEFAULT 0,
    ADD COLUMN last_breach_at_ms   bigint,
    ADD COLUMN last_breach_dim     text,
    ADD COLUMN next_reset_at_ms    bigint;

-- Partial index backing the `budget_reset` reconciler scan. Partial
-- on `next_reset_at_ms IS NOT NULL` so the index size scales with
-- the scheduled-reset subset, not the full policy set.
--
-- **RFC-vs-reality note.** RFC-020 Revision 6 §4.4.5 specifies
-- `CREATE INDEX CONCURRENTLY` for this index. Postgres 16 does NOT
-- support `CONCURRENTLY` on partitioned parents (`ff_budget_policy`
-- is HASH-partitioned since 0002) — the statement errors with
-- `cannot create index on partitioned table "ff_budget_policy"
-- concurrently`. Matching the 0001/0002 convention for partitioned
-- hot tables we use plain `CREATE INDEX`. The 0013 migration only
-- adds the `next_reset_at_ms` column in the SAME statement block; at
-- this point every existing row has `next_reset_at_ms = NULL` (the
-- default), so the partial-index build touches zero rows and the
-- AccessExclusiveLock on the parent is brief (metadata-only).
-- Post-0013 writes from Standalone-1 `create_budget` populate the
-- column on new rows only.
CREATE INDEX ff_budget_policy_reset_due_idx
    ON ff_budget_policy (partition_key, next_reset_at_ms)
    WHERE next_reset_at_ms IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    13,
    '0013_budget_policy_extensions',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
