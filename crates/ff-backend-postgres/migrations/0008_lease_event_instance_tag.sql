-- #282 — denormalised scanner-filter columns on `ff_lease_event`.
--
-- Mirrors the `namespace` / `instance_tag` columns already carried on
-- `ff_completion_event` (see `0001_initial.sql` §Section 8). The
-- subscriber (`lease_event_subscribe`) applies the caller-supplied
-- `ScannerFilter` as inline SQL predicates against these columns so
-- multiple FlowFabric consumers sharing a single Postgres backend can
-- tail disjoint subsets of the outbox without mutual interference.
--
-- Semantics:
--   * `namespace` — copy of `ff_exec_core.raw_fields->>'namespace'`
--     for the emitting execution, stamped at INSERT time.
--   * `instance_tag` — the VALUE of
--     `ff_exec_core.raw_fields->'tags'->>'cairn.instance_id'` if the
--     execution carries that tag, else NULL. Matches the
--     `dependency_reconciler::reconcile_tick` filter pattern
--     (`instance_tag = $value`); the consumer-side filter on
--     `ScannerFilter { instance_tag: Some((k, v)), .. }` compares
--     `v` to the column.
--
-- Additive (`backward_compatible = true`): existing rows default to
-- NULL; NULL matches no non-NULL filter, so pre-migration rows are
-- silently dropped by filtered subscribers. An unfiltered subscriber
-- (`ScannerFilter::default()`) ignores both columns and preserves
-- today's behaviour.

ALTER TABLE ff_lease_event
    ADD COLUMN namespace    TEXT,
    ADD COLUMN instance_tag TEXT;

CREATE INDEX ff_lease_event_namespace_idx
    ON ff_lease_event (partition_key, namespace)
    WHERE namespace IS NOT NULL;

CREATE INDEX ff_lease_event_instance_tag_idx
    ON ff_lease_event (partition_key, instance_tag)
    WHERE instance_tag IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    8,
    '0008_lease_event_instance_tag',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
