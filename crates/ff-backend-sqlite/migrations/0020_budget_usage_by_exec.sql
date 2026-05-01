-- RFC-023 — SQLite port of
-- `crates/ff-backend-postgres/migrations/0020_budget_usage_by_exec.sql`.
--
-- cairn #454 Phase 4a/5 — per-execution budget ledger (option A).
-- Flat table (no HASH partitioning, per RFC-023 §4.1 A3 single-writer).
-- uuid → TEXT; bigint → INTEGER; smallint → INTEGER.

CREATE TABLE ff_budget_usage_by_exec (
    partition_key   INTEGER NOT NULL,
    budget_id       TEXT    NOT NULL,
    execution_id    TEXT    NOT NULL,
    dimensions_key  TEXT    NOT NULL,
    delta_total     INTEGER NOT NULL DEFAULT 0,
    updated_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (partition_key, budget_id, execution_id, dimensions_key)
);

-- release_budget scan index: given (partition_key, budget_id,
-- execution_id), list all dimensions with delta_total for the
-- subsequent UPDATE (aggregate) + DELETE (ledger) pair.
CREATE INDEX ff_budget_usage_by_exec_release_idx
    ON ff_budget_usage_by_exec (partition_key, budget_id, execution_id);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    20,
    '0020_budget_usage_by_exec',
    CAST(strftime('%s','now') AS INTEGER) * 1000,
    1
);
