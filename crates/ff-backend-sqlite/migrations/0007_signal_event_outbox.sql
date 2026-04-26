-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0007_signal_event_outbox.sql`.
--
-- Schema + index intact. pg_notify trigger dropped — Rust post-commit
-- broadcast per RFC-023 §4.2.

CREATE TABLE ff_signal_event (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id    TEXT NOT NULL,
    signal_id       TEXT NOT NULL,
    waitpoint_id    TEXT,
    source_identity TEXT,
    delivered_at_ms INTEGER NOT NULL,
    partition_key   INTEGER NOT NULL
);

CREATE INDEX ff_signal_event_partition_key_idx
    ON ff_signal_event (partition_key, event_id);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    7,
    '0007_signal_event_outbox',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
