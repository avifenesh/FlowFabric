-- RFC-023 SQLite port of
-- `crates/ff-backend-postgres/migrations/0016_started_at_ms.sql`
-- (RFC-020 Wave 9 follow-up #356).
--
-- Adds `ff_exec_core.started_at_ms INTEGER` as a set-once column
-- populated on first claim; backfills from
-- `ff_attempt.started_at_ms` so the Spine-B read path can drop the
-- correlated-subquery LATERAL-equivalent.
--
-- Additive.

ALTER TABLE ff_exec_core
    ADD COLUMN started_at_ms INTEGER;

-- Backfill. `MIN(started_at_ms)` over non-NULL attempts per execution
-- matches the pre-0016 read path's `ORDER BY attempt_index ASC
-- LIMIT 1` over `started_at_ms IS NOT NULL`.
UPDATE ff_exec_core
   SET started_at_ms = (
        SELECT MIN(a.started_at_ms)
          FROM ff_attempt a
         WHERE a.partition_key = ff_exec_core.partition_key
           AND a.execution_id  = ff_exec_core.execution_id
           AND a.started_at_ms IS NOT NULL
   )
 WHERE started_at_ms IS NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    16,
    '0016_started_at_ms',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
