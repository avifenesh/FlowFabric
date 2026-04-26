-- RFC-020 Wave 9 — `ff_operator_event` outbox + NOTIFY trigger.
--
-- New outbox channel for operator-control events emitted by Spine-A
-- method impls (`cancel_execution` / `change_priority` /
-- `replay_execution` / `cancel_flow_header`). Mirrors
-- `ff_signal_event` (`0007_signal_event_outbox.sql`) +
-- `ff_lease_event` (`0006_lease_event_outbox.sql`) shape: not
-- partitioned, BIGSERIAL `event_id` PK, per-row `partition_key`,
-- `pg_notify('ff_operator_event', event_id::text)` AFTER INSERT
-- trigger.
--
-- `namespace` / `instance_tag` ship in the initial DDL (rather than
-- bolted on in a later 0009-shaped migration) per RFC-020 §4.3.1 —
-- Wave 9's consumer surface already expects RFC-019 subscriber
-- filters to work.
--
-- Additive (`backward_compatible = true`).

CREATE TABLE ff_operator_event (
    event_id        BIGSERIAL PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL
        CHECK (event_type IN ('priority_changed', 'replayed', 'flow_cancel_requested')),
    details         jsonb,
    occurred_at_ms  BIGINT NOT NULL,
    partition_key   INT NOT NULL,
    namespace       TEXT,
    instance_tag    TEXT
);

-- Subscriber catch-up: `WHERE partition_key = $1 AND event_id > $2`.
CREATE INDEX ff_operator_event_partition_key_idx
    ON ff_operator_event (partition_key, event_id);

-- RFC-019 subscriber-filters (parity with `0009_signal_event_instance_tag.sql`).
CREATE INDEX ff_operator_event_namespace_idx
    ON ff_operator_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_operator_event_instance_tag_idx
    ON ff_operator_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

CREATE FUNCTION ff_notify_operator_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('ff_operator_event', NEW.event_id::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_operator_event_notify_trg
    AFTER INSERT ON ff_operator_event
    FOR EACH ROW
    EXECUTE FUNCTION ff_notify_operator_event();

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    10,
    '0010_operator_event_outbox',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
