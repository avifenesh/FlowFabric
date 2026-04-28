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

-- Backfill using the same semantics as the pre-0016 read path:
-- pick `started_at_ms` from the earliest attempt row by
-- `attempt_index ASC` with a non-NULL `started_at_ms`. MIN over the
-- timestamp would drift if values are ever non-monotonic; ordering on
-- `attempt_index` preserves the exact prior behaviour. The EXISTS
-- guard skips exec rows without any matching attempt so the UPDATE
-- doesn't touch (and re-NULL) submit-but-never-claimed rows on large
-- tables.
UPDATE ff_exec_core
   SET started_at_ms = (
        SELECT a.started_at_ms
          FROM ff_attempt a
         WHERE a.partition_key = ff_exec_core.partition_key
           AND a.execution_id  = ff_exec_core.execution_id
           AND a.started_at_ms IS NOT NULL
         ORDER BY a.attempt_index ASC
         LIMIT 1
   )
 WHERE started_at_ms IS NULL
   AND EXISTS (
        SELECT 1 FROM ff_attempt a
         WHERE a.partition_key = ff_exec_core.partition_key
           AND a.execution_id  = ff_exec_core.execution_id
           AND a.started_at_ms IS NOT NULL
   );

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    16,
    '0016_started_at_ms',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
