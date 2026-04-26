-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0014_cancel_backlog.sql`.
--
-- Flat tables; no partitioning. uuid → BLOB.

CREATE TABLE ff_cancel_backlog (
    partition_key        INTEGER NOT NULL,
    flow_id              BLOB    NOT NULL,
    requested_at_ms      INTEGER NOT NULL,
    requester            TEXT    NOT NULL DEFAULT '',
    reason               TEXT    NOT NULL DEFAULT '',
    grace_until_ms       INTEGER,
    cancellation_policy  TEXT    NOT NULL,
    status               TEXT    NOT NULL DEFAULT 'pending',
    PRIMARY KEY (partition_key, flow_id)
);

CREATE TABLE ff_cancel_backlog_member (
    partition_key   INTEGER NOT NULL,
    flow_id         BLOB    NOT NULL,
    execution_id    TEXT    NOT NULL,
    PRIMARY KEY (partition_key, flow_id, execution_id)
);

CREATE INDEX ff_cancel_backlog_grace_idx
    ON ff_cancel_backlog (partition_key, grace_until_ms)
    WHERE grace_until_ms IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    14,
    '0014_cancel_backlog',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
