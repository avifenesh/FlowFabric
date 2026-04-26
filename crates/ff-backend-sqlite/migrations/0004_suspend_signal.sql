-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0004_suspend_signal.sql`.
--
-- Additive. Same shape; jsonb → TEXT.

CREATE TABLE ff_suspend_dedup (
    partition_key    INTEGER NOT NULL,
    idempotency_key  TEXT    NOT NULL,
    outcome_json     TEXT    NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (partition_key, idempotency_key)
);

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN waitpoint_key TEXT NOT NULL DEFAULT '';

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    4,
    '0004_suspend_signal',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
