-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0006_lease_event_outbox.sql`.
--
-- Schema + index intact (subscribers still drain via
-- `event_id > $cursor`). pg_notify trigger is dropped; broadcast
-- moves to the in-Rust post-commit channel per RFC-023 §4.2.

CREATE TABLE ff_lease_event (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id    TEXT NOT NULL,
    lease_id        TEXT,
    event_type      TEXT NOT NULL,
    occurred_at_ms  INTEGER NOT NULL,
    partition_key   INTEGER NOT NULL
);

CREATE INDEX ff_lease_event_partition_key_idx
    ON ff_lease_event (partition_key, event_id);

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    6,
    '0006_lease_event_outbox',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
