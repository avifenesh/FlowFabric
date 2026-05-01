-- no-transaction
-- cairn #454 Phase 4a — per-execution budget ledger (option A).
--
-- Adds `ff_budget_usage_by_exec`, the per-execution attribution
-- ledger that backs `record_spend` + `release_budget` on Postgres
-- (parity with Valkey PR #464's `ff:budget:...:by_exec:<exec>` HASH).
-- `record_spend` UPSERTs into this table alongside the `ff_budget_usage`
-- aggregate; `release_budget` scans rows for a single execution,
-- subtracts the `delta_total` from the aggregate (clamped at 0), and
-- DELETEs the per-exec rows.
--
-- Partitioned by HASH(partition_key) with 256 children to match
-- `ff_budget_usage` / `ff_budget_policy` (Wave 3b §Q5). Partition-
-- child DDL uses one DO block with an explicit COMMIT boundary to
-- stay under `max_locks_per_transaction` (same discipline as
-- 0002_budget.sql §Section 2).

CREATE TABLE ff_budget_usage_by_exec (
    partition_key   smallint NOT NULL,
    budget_id       text     NOT NULL,
    execution_id    uuid     NOT NULL,
    dimensions_key  text     NOT NULL,
    delta_total     bigint   NOT NULL DEFAULT 0,
    updated_at_ms   bigint   NOT NULL,
    PRIMARY KEY (partition_key, budget_id, execution_id, dimensions_key)
) PARTITION BY HASH (partition_key);

COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_budget_usage_by_exec FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_budget_usage_by_exec_p' || i, i); END LOOP; END $$;
COMMIT;

-- release_budget scan index: given (partition_key, budget_id,
-- execution_id), list all dimensions with delta_total for SELECT
-- FOR UPDATE + subsequent DELETE.
CREATE INDEX ff_budget_usage_by_exec_release_idx
    ON ff_budget_usage_by_exec (partition_key, budget_id, execution_id);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    20,
    '0020_budget_usage_by_exec',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
