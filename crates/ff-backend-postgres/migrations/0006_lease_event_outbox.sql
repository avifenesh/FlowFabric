-- RFC-019 Stage B — `ff_lease_event` outbox + NOTIFY trigger.
--
-- Mirrors `ff_completion_event` (see `0001_initial.sql` §Section 8):
-- producers INSERT a row per lease lifecycle transition in the same
-- transaction as the state change; the trigger fires
-- `pg_notify('ff_lease_event', event_id::text)` at COMMIT so
-- subscribers wake and drain newly-committed rows via the
-- `event_id > $cursor` catch-up query.
--
-- Additive (`backward_compatible = true`).

CREATE TABLE ff_lease_event (
    event_id        BIGSERIAL PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    lease_id        TEXT,
    event_type      TEXT NOT NULL,
        -- 'acquired' / 'renewed' / 'expired' / 'reclaimed' / 'revoked'
    occurred_at_ms  BIGINT NOT NULL,
    partition_key   INT NOT NULL
);

-- Subscriber catch-up: `WHERE partition_key = $1 AND event_id > $2`.
CREATE INDEX ff_lease_event_partition_key_idx
    ON ff_lease_event (partition_key, event_id);

CREATE FUNCTION ff_notify_lease_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('ff_lease_event', NEW.event_id::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_lease_event_notify_trg
    AFTER INSERT ON ff_lease_event
    FOR EACH ROW
    EXECUTE FUNCTION ff_notify_lease_event();

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    6,
    '0006_lease_event_outbox',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
