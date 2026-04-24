-- no-transaction
-- RFC-v0.7 Wave 3b — budget schema (RFC-008).
--
-- Wave 3's `0001_initial.sql` deliberately omitted budget tables;
-- Wave 4f (budget proc impls) is blocked without them. This
-- migration lands the three budget parent tables + their 256-way
-- hash partition children, and annotates itself as
-- `backward_compatible=true` (additive — existing Wave 4 impls
-- ignore these tables).
--
-- Authority:
--   * rfcs/RFC-008-budget.md — budget semantics.
--   * rfcs/drafts/v0.7-migration-master.md §Q5 (HASH 256
--     partitioning, partition_key sole hash input),
--     §Q11 (budget ops run at READ COMMITTED + row-level
--     `FOR UPDATE`, not SERIALIZABLE — relevant for Wave 4f procs,
--     not this DDL), §Q12 (backward_compatible annotation).
--
-- Schema conventions (mirrors 0001_initial.sql §Schema conventions):
--   * PARTITION BY HASH (partition_key) with 256 children; same
--     hash input + count as Wave 3 for uniformity.
--   * Timestamps stored as `*_ms bigint`.
--   * JSONB for policy / outcome snapshot shapes that Wave 4f owns.
--   * PKs on partitioned tables include partition_key.
--
-- Partition-child creation uses one `DO` block per parent with
-- explicit COMMIT boundaries to stay under
-- `max_locks_per_transaction` on stock Postgres — identical pattern
-- to 0001's §Section 7. The leading `-- no-transaction` directive
-- opts sqlx out of wrapping this file in one txn.

-- ============================================================
-- Section 1 — Budget parent tables (partitioned)
-- ============================================================

-- ff_budget_policy — definitional rows (one per budget_id). The
-- `policy_json` blob holds hard_limit / soft_limit / reset_policy /
-- etc; Wave 4f procs own the shape.
CREATE TABLE ff_budget_policy (
    partition_key  smallint NOT NULL,
    budget_id      text     NOT NULL,
    policy_json    jsonb    NOT NULL,
    created_at_ms  bigint   NOT NULL,
    updated_at_ms  bigint   NOT NULL,
    PRIMARY KEY (partition_key, budget_id)
) PARTITION BY HASH (partition_key);

-- ff_budget_usage — running totals, one row per
-- (budget_id, dimensions_key). `dimensions_key` is the canonical-
-- sorted stringification of `UsageDimensions` (same key shape used
-- by Valkey's per-dimension Hash) so the Wave 4f check/report procs
-- can key lookups without unpacking the full JSON.
CREATE TABLE ff_budget_usage (
    partition_key     smallint NOT NULL,
    budget_id         text     NOT NULL,
    dimensions_key    text     NOT NULL,
    current_value     bigint   NOT NULL DEFAULT 0,
    last_reset_at_ms  bigint,
    updated_at_ms     bigint   NOT NULL,
    PRIMARY KEY (partition_key, budget_id, dimensions_key)
) PARTITION BY HASH (partition_key);

-- ff_budget_usage_dedup — idempotency cache for `report_usage`
-- (RFC-012 §R7.2.3). `dedup_key` is caller-supplied; when a
-- duplicate call arrives the cached `outcome_json` is replayed
-- verbatim. `expires_at_ms` bounds retention; a future TTL sweeper
-- (separate migration) walks the index below to evict expired rows.
CREATE TABLE ff_budget_usage_dedup (
    partition_key  smallint NOT NULL,
    dedup_key      text     NOT NULL,
    outcome_json   jsonb    NOT NULL,
    applied_at_ms  bigint   NOT NULL,
    expires_at_ms  bigint   NOT NULL,
    PRIMARY KEY (partition_key, dedup_key)
) PARTITION BY HASH (partition_key);

-- TTL-sweeper scan index.
CREATE INDEX ff_budget_usage_dedup_expires_idx
    ON ff_budget_usage_dedup (partition_key, expires_at_ms);

-- ============================================================
-- Section 2 — 256-way HASH partition children
-- ============================================================
-- 3 parents × 256 children = 768 partitions. One `DO` block per
-- parent with explicit COMMIT boundaries between blocks — same
-- lock-budget discipline as 0001 §Section 7.

COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_budget_policy FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_budget_policy_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_budget_usage FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_budget_usage_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_budget_usage_dedup FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_budget_usage_dedup_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 3 — Migration annotation (Q12)
-- ============================================================
-- backward_compatible=true: this migration is purely additive;
-- existing Wave 4 impls can ignore these tables.

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    2,
    '0002_budget',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
