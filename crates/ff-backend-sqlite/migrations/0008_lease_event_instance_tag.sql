-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0008_lease_event_instance_tag.sql`.
--
-- SQLite does not support multiple ADD COLUMNs in a single ALTER;
-- split into two statements.

ALTER TABLE ff_lease_event
    ADD COLUMN namespace    TEXT;

ALTER TABLE ff_lease_event
    ADD COLUMN instance_tag TEXT;

CREATE INDEX ff_lease_event_namespace_idx
    ON ff_lease_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_lease_event_instance_tag_idx
    ON ff_lease_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    8,
    '0008_lease_event_instance_tag',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
