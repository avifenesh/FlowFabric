-- PR-7b Cluster 4: index `ff_completion_event` lookups by
-- (partition_key, execution_id, occurred_at_ms).
--
-- `resolve_event_id` (ff-backend-postgres::lib.rs) runs a per-payload
-- point lookup on this key shape from the push-based completion
-- listener — every cascade call pays this query. Without an index the
-- planner falls back to scanning the matching partition of
-- `ff_completion_event` for each payload, which turns cascade dispatch
-- into an O(rows_in_partition) hot-path cost under load.
--
-- The composite (partition_key, execution_id, occurred_at_ms) is
-- scoped to one hash-partition and matches the predicate column-for-
-- column, so the planner can drive the `ORDER BY event_id ASC LIMIT 1`
-- via an index scan on the per-partition local index.
--
-- `IF NOT EXISTS` is defensive: in case a deployment already applied
-- this via a manual DBA-side fix, the migration is still safely
-- idempotent.

CREATE INDEX IF NOT EXISTS ff_completion_event_lookup_idx
    ON ff_completion_event (partition_key, execution_id, occurred_at_ms);
