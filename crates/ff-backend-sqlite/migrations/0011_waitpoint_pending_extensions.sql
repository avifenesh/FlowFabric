-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0011_waitpoint_pending_extensions.sql`.
--
-- Additive. TEXT[] → TEXT holding a JSON array literal; SQLite's
-- ALTER TABLE only supports one ADD COLUMN per statement.

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN required_signal_names TEXT NOT NULL DEFAULT '[]';

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN activated_at_ms INTEGER;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    11,
    '0011_waitpoint_pending_extensions',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
