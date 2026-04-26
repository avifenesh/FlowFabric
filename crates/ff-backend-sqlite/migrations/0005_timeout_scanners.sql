-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0005_timeout_scanners.sql`.
--
-- Additive. Single ALTER.

ALTER TABLE ff_suspension_current
    ADD COLUMN timeout_behavior TEXT;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    5,
    '0005_timeout_scanners',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
