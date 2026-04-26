-- #282 — denormalised scanner-filter columns on `ff_signal_event`.
--
-- Mirror of `0008_lease_event_instance_tag.sql` against the signal
-- delivery outbox. See the sibling migration for the semantics +
-- rationale.

ALTER TABLE ff_signal_event
    ADD COLUMN namespace    TEXT,
    ADD COLUMN instance_tag TEXT;

CREATE INDEX ff_signal_event_namespace_idx
    ON ff_signal_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_signal_event_instance_tag_idx
    ON ff_signal_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    9,
    '0009_signal_event_instance_tag',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
