-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0010_operator_event_outbox.sql`.
--
-- Schema + indexes intact (including the event_type CHECK — SQLite
-- supports CHECK). pg_notify trigger dropped; Rust post-commit
-- broadcast per RFC-023 §4.2. jsonb → TEXT.

CREATE TABLE ff_operator_event (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL
        CHECK (event_type IN ('priority_changed', 'replayed', 'flow_cancel_requested')),
    details         TEXT,
    occurred_at_ms  INTEGER NOT NULL,
    partition_key   INTEGER NOT NULL,
    namespace       TEXT,
    instance_tag    TEXT
);

CREATE INDEX ff_operator_event_partition_key_idx
    ON ff_operator_event (partition_key, event_id);

CREATE INDEX ff_operator_event_namespace_idx
    ON ff_operator_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_operator_event_instance_tag_idx
    ON ff_operator_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    10,
    '0010_operator_event_outbox',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
