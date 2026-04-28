-- RFC-024 PR-E — SQLite `ff_claim_grant` table + `ff_exec_core.lease_reclaim_count`
--
-- SQLite-dialect port of PR-D's Postgres migration. RFC-023 §4.1: no
-- partitioning (N=1 supervisor), BLOB for raw bytes, TEXT JSON for
-- capabilities (SQLite has no JSONB + no array type).
--
-- Unlike the PG migration this port does NOT backfill JSON-stashed
-- grants: SQLite never shipped a scheduler.rs `claim_grant` JSON stash
-- (RFC-023 §5 — `claim_for_worker` is a permanent non-goal on the
-- dev-only backend). Fresh DBs + existing DBs alike start with an
-- empty `ff_claim_grant` table.
--
-- `lease_reclaim_count` on `ff_exec_core` tracks how many times the
-- execution has been reclaimed via `reclaim_execution` (RFC-024 §3.3).
-- Required for the §9 release-gate test (cap exceeded at 1000).

CREATE TABLE ff_claim_grant (
    partition_key        INTEGER NOT NULL,
    grant_id             BLOB    NOT NULL,
    execution_id         BLOB    NOT NULL,
    kind                 TEXT    NOT NULL
        CHECK (kind IN ('claim', 'reclaim')),
    worker_id            TEXT    NOT NULL,
    worker_instance_id   TEXT    NOT NULL,
    lane_id              TEXT,
    capability_hash      TEXT,
    worker_capabilities  TEXT    NOT NULL DEFAULT '[]',
    route_snapshot_json  TEXT,
    admission_summary    TEXT,
    grant_ttl_ms         INTEGER NOT NULL,
    issued_at_ms         INTEGER NOT NULL,
    expires_at_ms        INTEGER NOT NULL,
    PRIMARY KEY (partition_key, grant_id)
);

CREATE INDEX ff_claim_grant_execution_idx
    ON ff_claim_grant (partition_key, execution_id);

CREATE INDEX ff_claim_grant_expiry_idx
    ON ff_claim_grant (expires_at_ms);

ALTER TABLE ff_exec_core
    ADD COLUMN lease_reclaim_count INTEGER NOT NULL DEFAULT 0;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    17,
    '0017_claim_grant_table',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
