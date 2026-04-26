-- RFC-020 Wave 9 — `ff_waitpoint_pending` additive columns for
-- `list_pending_waitpoints` (Standalone-2).
--
-- The real `PendingWaitpointInfo` contract
-- (`crates/ff-core/src/contracts/mod.rs:822-859`) has 10 fields; of
-- those, three have no source column in the pre-Wave-9 schema:
-- `state`, `required_signal_names`, `activated_at`. This migration
-- adds them as additive, defaulted columns.
--
-- (`waitpoint_key` — a fourth contract field previously scoped into
-- 0011 in RFC-020 Revision 3/4 — already exists on
-- `ff_waitpoint_pending` as `TEXT NOT NULL DEFAULT ''` since
-- migration 0004 (`0004_suspend_signal.sql:33-34`) and is populated
-- by `suspend_ops.rs`. RFC-020 Revision 5 corrected the scope.)
--
-- Existing rows inserted pre-0011 get `state = 'pending'`, empty
-- `required_signal_names`, NULL `activated_at_ms`. New inserts
-- (post-0011) populate `required_signal_names` from the
-- producer-side `suspend_*` path; activation transitions populate
-- `state = 'active', activated_at_ms = now`. Producer + activation
-- wiring changes land in the same PR as this migration (RFC-020
-- §3.1.1, §6.1 step 4). This migration is schema-only.
--
-- Additive (`backward_compatible = true`).

ALTER TABLE ff_waitpoint_pending
    ADD COLUMN state                 TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN required_signal_names TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN activated_at_ms       BIGINT;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    11,
    '0011_waitpoint_pending_extensions',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
