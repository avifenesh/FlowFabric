-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0009_signal_event_instance_tag.sql`.
--
-- SQLite ALTER TABLE only supports one ADD COLUMN per statement.

ALTER TABLE ff_signal_event
    ADD COLUMN namespace    TEXT;

ALTER TABLE ff_signal_event
    ADD COLUMN instance_tag TEXT;

CREATE INDEX ff_signal_event_namespace_idx
    ON ff_signal_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_signal_event_instance_tag_idx
    ON ff_signal_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    9,
    '0009_signal_event_instance_tag',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
