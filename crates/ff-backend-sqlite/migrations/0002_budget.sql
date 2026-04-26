-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0002_budget.sql`.
--
-- Flat tables (no HASH partitioning, per RFC-023 §4.1). jsonb → TEXT;
-- bigint → INTEGER; smallint → INTEGER.

CREATE TABLE ff_budget_policy (
    partition_key  INTEGER NOT NULL,
    budget_id      TEXT    NOT NULL,
    policy_json    TEXT    NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (partition_key, budget_id)
);

CREATE TABLE ff_budget_usage (
    partition_key     INTEGER NOT NULL,
    budget_id         TEXT    NOT NULL,
    dimensions_key    TEXT    NOT NULL,
    current_value     INTEGER NOT NULL DEFAULT 0,
    last_reset_at_ms  INTEGER,
    updated_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (partition_key, budget_id, dimensions_key)
);

CREATE TABLE ff_budget_usage_dedup (
    partition_key  INTEGER NOT NULL,
    dedup_key      TEXT    NOT NULL,
    outcome_json   TEXT    NOT NULL,
    applied_at_ms  INTEGER NOT NULL,
    expires_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (partition_key, dedup_key)
);

CREATE INDEX ff_budget_usage_dedup_expires_idx
    ON ff_budget_usage_dedup (partition_key, expires_at_ms);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    2,
    '0002_budget',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
