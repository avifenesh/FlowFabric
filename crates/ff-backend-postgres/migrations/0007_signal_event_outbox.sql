-- RFC-019 Stage B — `ff_signal_event` outbox + NOTIFY trigger.
--
-- Mirrors `ff_lease_event` (see `0006_lease_event_outbox.sql`):
-- producers INSERT a row per successful `deliver_signal` call in the
-- same SERIALIZABLE transaction as the signal-state write; the trigger
-- fires `pg_notify('ff_signal_event', event_id::text)` at COMMIT so
-- subscribers wake and drain newly-committed rows via the
-- `event_id > $cursor` catch-up query.
--
-- Additive (`backward_compatible = true`).

CREATE TABLE ff_signal_event (
    event_id        BIGSERIAL PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    signal_id       TEXT NOT NULL,
    waitpoint_id    TEXT,
    source_identity TEXT,
    delivered_at_ms BIGINT NOT NULL,
    partition_key   INT NOT NULL
);

-- Subscriber catch-up: `WHERE partition_key = $1 AND event_id > $2`.
CREATE INDEX ff_signal_event_partition_key_idx
    ON ff_signal_event (partition_key, event_id);

CREATE FUNCTION ff_notify_signal_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('ff_signal_event', NEW.event_id::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_signal_event_notify_trg
    AFTER INSERT ON ff_signal_event
    FOR EACH ROW
    EXECUTE FUNCTION ff_notify_signal_event();

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    7,
    '0007_signal_event_outbox',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
