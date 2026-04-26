-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0003_dispatch_outbox.sql`.
--
-- Additive; same shape on SQLite.

ALTER TABLE ff_completion_event
    ADD COLUMN dispatched_at_ms INTEGER;

CREATE INDEX ff_completion_event_pending_dispatch_idx
    ON ff_completion_event (partition_key, committed_at_ms)
    WHERE dispatched_at_ms IS NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    3,
    '0003_dispatch_outbox',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
