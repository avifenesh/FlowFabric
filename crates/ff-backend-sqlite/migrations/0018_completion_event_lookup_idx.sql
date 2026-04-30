-- PR-7b Cluster 4: index `ff_completion_event` lookups by
-- (partition_key, execution_id, occurred_at_ms) — SQLite sibling of
-- the Postgres `0018_completion_event_lookup_idx.sql`.
--
-- The Postgres backend's `resolve_event_id` runs a per-payload point
-- lookup on this key shape from the push-based completion listener;
-- the SQLite backend doesn't share that code path today, but parity
-- with the PG schema keeps the two migration trees in lock-step per
-- RFC-023 §4.1 (and future SQLite completion-cascade work lands
-- against the same index shape).

CREATE INDEX IF NOT EXISTS ff_completion_event_lookup_idx
    ON ff_completion_event (partition_key, execution_id, occurred_at_ms);
