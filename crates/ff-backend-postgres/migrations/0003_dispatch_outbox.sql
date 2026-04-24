-- Wave 5a — cascade dispatch idempotency column.
--
-- The `dispatch_dependency_resolution` cascade claims a completion
-- outbox row by flipping `dispatched_at_ms` from NULL to a timestamp
-- inside a one-row UPDATE...RETURNING.  A replay of the same
-- event_id sees a non-NULL `dispatched_at_ms` and short-circuits
-- with `DispatchOutcome::NoOp`.  The reconciler (Wave 6) scans for
-- rows where `dispatched_at_ms IS NULL AND committed_at_ms < now() -
-- threshold` and re-invokes the dispatcher.
--
-- A single column addition; Postgres applies ALTER COLUMN to the
-- partitioned parent which propagates to every child partition.

ALTER TABLE ff_completion_event
    ADD COLUMN dispatched_at_ms bigint;

-- Reconciler scan: rows whose commit is older than a threshold and
-- have never been dispatched.  Partial index — small, hot-path.
CREATE INDEX ff_completion_event_pending_dispatch_idx
    ON ff_completion_event (partition_key, committed_at_ms)
    WHERE dispatched_at_ms IS NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    3,
    '0003_dispatch_outbox',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
