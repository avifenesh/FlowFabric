-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0012_quota_policy.sql`.
--
-- Flat tables; no HASH-partitioning DDL on SQLite (RFC-023 §4.1).

CREATE TABLE ff_quota_policy (
    partition_key                 INTEGER NOT NULL,
    quota_policy_id               TEXT    NOT NULL,
    requests_per_window_seconds   INTEGER NOT NULL,
    max_requests_per_window       INTEGER NOT NULL,
    active_concurrency_cap        INTEGER NOT NULL,
    active_concurrency            INTEGER NOT NULL DEFAULT 0,
    created_at_ms                 INTEGER NOT NULL,
    updated_at_ms                 INTEGER NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id)
);

CREATE TABLE ff_quota_window (
    partition_key     INTEGER NOT NULL,
    quota_policy_id   TEXT    NOT NULL,
    dimension         TEXT    NOT NULL DEFAULT '',
    admitted_at_ms    INTEGER NOT NULL,
    execution_id      TEXT    NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, dimension, admitted_at_ms, execution_id)
);

CREATE TABLE ff_quota_admitted (
    partition_key     INTEGER NOT NULL,
    quota_policy_id   TEXT    NOT NULL,
    execution_id      TEXT    NOT NULL,
    admitted_at_ms    INTEGER NOT NULL,
    expires_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, execution_id)
);

CREATE INDEX ff_quota_admitted_expires_idx
    ON ff_quota_admitted (partition_key, expires_at_ms);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    12,
    '0012_quota_policy',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
