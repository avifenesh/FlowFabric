-- no-transaction
-- RFC-020 Wave 9 Standalone-1 — quota-policy schema (Revision 6 §4.4.1).
--
-- Creates the three quota tables the Standalone-1 admin methods + a
-- future admission impl need:
--
--   * `ff_quota_policy`  — definitional row per policy (+
--     `active_concurrency` counter column folding what Valkey holds
--     in a dedicated :concurrency key).
--   * `ff_quota_window`  — sliding-window admission records (one row
--     per (policy, dim, admitted_at_ms, execution_id)).
--   * `ff_quota_admitted` — admitted-set tracking for idempotent
--     admission guards (mirrors Valkey's admitted_set SADD).
--
-- All three tables are HASH-partitioned 256 ways on `partition_key`
-- matching the 0001/0002 hot-table convention. Partition-child
-- creation uses one `DO` block per parent with explicit COMMIT
-- boundaries between blocks to stay under
-- `max_locks_per_transaction` — identical pattern to 0002 §Section 2.
--
-- Reviewer finding (gemini-code-assist, PR #350 Round 6 #2):
-- `ff_quota_window_sweep_idx` is redundant — the PK
-- `(partition_key, quota_policy_id, dimension, admitted_at_ms,
-- execution_id)` already supports sweep scans as a B-tree prefix
-- match. No separate sweep index.
--
-- Authority:
--   * rfcs/RFC-020-postgres-wave-9.md §4.4.1 + §7.12
--   * crates/ff-script/src/flowfabric.lua:6839-6878 (ff_create_quota_policy)

-- ============================================================
-- Section 1 — Parent tables
-- ============================================================

-- Policy definition (one row per policy). `active_concurrency` folds
-- Valkey's dedicated concurrency counter key onto a column on the
-- policy row; admission-rate contention is analysed in RFC §4.4.4 +
-- §7.12 (splitting to a separate sharded-counter table is the
-- follow-up migration if it surfaces).
CREATE TABLE ff_quota_policy (
    partition_key                 smallint NOT NULL,
    quota_policy_id               text     NOT NULL,
    requests_per_window_seconds   bigint   NOT NULL,
    max_requests_per_window       bigint   NOT NULL,
    active_concurrency_cap        bigint   NOT NULL,
    active_concurrency            bigint   NOT NULL DEFAULT 0,
    created_at_ms                 bigint   NOT NULL,
    updated_at_ms                 bigint   NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id)
) PARTITION BY HASH (partition_key);

-- Sliding-window admission record. One row per admitted request per
-- (policy, dimension). ZREMRANGEBYSCORE maps to DELETE WHERE
-- admitted_at_ms < now - window_ms — sweep scans use the PK as a
-- B-tree prefix match; no dedicated sweep index (review #2).
CREATE TABLE ff_quota_window (
    partition_key     smallint NOT NULL,
    quota_policy_id   text     NOT NULL,
    dimension         text     NOT NULL DEFAULT '',
    admitted_at_ms    bigint   NOT NULL,
    execution_id      text     NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, dimension, admitted_at_ms, execution_id)
) PARTITION BY HASH (partition_key);

-- Idempotent admitted-set: presence = admitted-within-current-window.
-- Mirrors Valkey's SADD admitted_set. `expires_at_ms` bounds the row
-- for the admitted-guard TTL semantic (Valkey's `admitted_guard_key`
-- SET NX PX window_ms) — a sweeper trims expired rows.
CREATE TABLE ff_quota_admitted (
    partition_key     smallint NOT NULL,
    quota_policy_id   text     NOT NULL,
    execution_id      text     NOT NULL,
    admitted_at_ms    bigint   NOT NULL,
    expires_at_ms     bigint   NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, execution_id)
) PARTITION BY HASH (partition_key);

-- ============================================================
-- Section 2 — 256-way HASH partition children
-- ============================================================
-- 3 parents × 256 children = 768 partitions. One `DO` block per parent
-- with explicit COMMIT boundaries — same lock-budget discipline as
-- 0001 §Section 7 + 0002 §Section 2.

COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_quota_policy FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_quota_policy_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_quota_window FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_quota_window_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_quota_admitted FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_quota_admitted_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 3 — Secondary indexes (non-PK)
-- ============================================================
-- Sweep index on `ff_quota_admitted(expires_at_ms)` for the TTL
-- sweeper (future reconciler; not wired in Wave 9).
-- Plain `CREATE INDEX` is safe here because these partition children
-- are zero-row post-create; `CONCURRENTLY` on partitioned parents is
-- not supported on Postgres 16 and unnecessary on empty tables.
CREATE INDEX ff_quota_admitted_expires_idx
    ON ff_quota_admitted (partition_key, expires_at_ms);

-- ============================================================
-- Section 4 — Migration annotation (Q12)
-- ============================================================
INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    12,
    '0012_quota_policy',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
