-- RFC-025 Phase 6 follow-up — fix event PK collision on same-ms events.
--
-- 0021 defined the event PK as (partition_key, namespace,
-- worker_instance_id, event_at_ms). Two events landing in the same
-- millisecond for the same worker (e.g. a fast register → heartbeat
-- pair on boot) collide and the second drops via the
-- `ON CONFLICT DO NOTHING` fallback in the insert bodies. Adding
-- `event_kind` to the PK distinguishes the two.
--
-- Drop + recreate is safe: the event table is additive audit trail,
-- there's no expected FK inbound, and the table is <24 h old in any
-- running deployment. The `INSERT ... ON CONFLICT DO NOTHING` call
-- sites already use the PK as the conflict target, so they remain
-- valid post-migration.

ALTER TABLE ff_worker_registry_event
  DROP CONSTRAINT ff_worker_registry_event_pkey;

ALTER TABLE ff_worker_registry_event
  ADD PRIMARY KEY (partition_key, namespace, worker_instance_id, event_at_ms, event_kind);
