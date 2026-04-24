-- RFC-v0.7 Wave 4d follow-up — suspend + deliver_signal support tables.
--
-- Additive (`backward_compatible = true`). Wave 4d landed the HMAC
-- rotation primitive + composite evaluator; the SQL bodies for
-- `suspend`, `deliver_signal`, `claim_resumed_execution`, and
-- `observe_signals` need two schema extensions this migration lands:
--
--   1. `ff_suspend_dedup` — partition-scoped idempotency cache for
--      `SuspendArgs::idempotency_key` replay (RFC-013 §2.2). Stores
--      the first call's serialised `SuspendOutcome` verbatim; a second
--      `suspend` with the same `(partition_key, idempotency_key)` pair
--      returns the cached outcome rather than re-minting waitpoints.
--
--   2. `ff_waitpoint_pending.waitpoint_key` — the human-readable
--      waitpoint key ("wpk:<uuid>") that `deliver_signal` needs to
--      look up when appending a signal into
--      `ff_suspension_current.member_map` (the composite evaluator
--      keys signals by waitpoint_key, not waitpoint_id).
--
-- Authority: brief for Wave 4d follow-up + RFC-013/014 §2.2/§2.4.
-- Q11 isolation: the dedup table is read+written inside the same
-- SERIALIZABLE `suspend` transaction; no extra index needed beyond
-- the composite primary key.

CREATE TABLE ff_suspend_dedup (
    partition_key    smallint NOT NULL,
    idempotency_key  text     NOT NULL,
    outcome_json     jsonb    NOT NULL,
    created_at_ms    bigint   NOT NULL,
    PRIMARY KEY (partition_key, idempotency_key)
);

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN waitpoint_key text NOT NULL DEFAULT '';

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    3,
    '0003_suspend_signal',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
