-- RFC-020 Wave 9 follow-up (#356) — promote first-claim timestamp
-- to a typed column on `ff_exec_core`.
--
-- Pre-0016 the Spine-B `read_execution_info` path recovered
-- `ExecutionInfo.started_at` via a LATERAL join on `ff_attempt` —
-- `SELECT started_at_ms FROM ff_attempt WHERE ... AND
-- started_at_ms IS NOT NULL ORDER BY attempt_index ASC LIMIT 1`.
-- That matched Valkey semantics (first-claim, preserved across
-- retries) but cost a correlated subquery per read.
--
-- This migration adds `ff_exec_core.started_at_ms BIGINT` as a
-- set-once column populated on first claim (see
-- `attempt.rs::claim` — post-0016 writes use
-- `started_at_ms = COALESCE(started_at_ms, $now)` so reclaim and retry
-- paths never overwrite the original first-claim timestamp). Existing
-- rows are backfilled from `ff_attempt.started_at_ms` (earliest
-- non-NULL attempt by `attempt_index ASC`) so the Spine-B read path
-- can drop the LATERAL join without regressing pre-existing
-- executions.
--
-- Migration numbering: 0015 is reserved for RFC-024 (claim-grant
-- table) which is queued but not yet shipped. This migration claims
-- 0016 to avoid collision; if the RFC-024 impl PR lands first it
-- takes 0015 and this file continues to apply cleanly. If this PR
-- lands first, the RFC-024 migration PR adjusts.
--
-- Additive (`backward_compatible = true`). No destructive DDL.

-- Section 1 — Add the column.
ALTER TABLE ff_exec_core
    ADD COLUMN IF NOT EXISTS started_at_ms BIGINT;

-- Section 2 — Backfill from existing `ff_attempt.started_at_ms`.
-- Uses DISTINCT ON to pick the earliest attempt row by
-- `attempt_index ASC` with a non-NULL started_at_ms, matching the
-- pre-0016 LATERAL-join selection exactly (`ORDER BY attempt_index
-- ASC LIMIT 1`). MIN(started_at_ms) would drift if timestamps were
-- ever non-monotonic. Rows whose executions never reached claim
-- (pure submit / cancel-before-claim) leave `started_at_ms` NULL,
-- which the read path already tolerates
-- (`started_at_ms_opt.map(...)` → `Option<String>`).
UPDATE ff_exec_core ec
   SET started_at_ms = sub.first_started_at_ms
  FROM (
    SELECT DISTINCT ON (a.partition_key, a.execution_id)
           a.partition_key,
           a.execution_id,
           a.started_at_ms AS first_started_at_ms
      FROM ff_attempt a
     WHERE a.started_at_ms IS NOT NULL
     ORDER BY a.partition_key, a.execution_id, a.attempt_index ASC
  ) sub
 WHERE ec.partition_key = sub.partition_key
   AND ec.execution_id  = sub.execution_id
   AND ec.started_at_ms IS NULL;

-- Section 3 — Migration annotation.
INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    16,
    '0016_started_at_ms',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
