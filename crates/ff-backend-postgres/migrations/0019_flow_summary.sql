-- no-transaction
-- PR-7b Cluster 2b-B — flow summary projection table.
--
-- Mirrors the Valkey `ff:flow:{fp:N}:<flow_id>:summary` hash shape so
-- `EngineBackend::project_flow_summary` can hydrate the same consumer-
-- visible rollup fields on Postgres. Columns match the HSET field names
-- from `lua/flow.lua` / the pre-PR-7b Rust scanner path.
--
-- Distinct from `ff_flow_core.public_flow_state`:
--   * `ff_flow_core.public_flow_state` — authoritative, owned by
--     `ff_create_flow` / `ff_cancel_flow`, used for mutation guards.
--   * `ff_flow_summary.public_flow_state` — DERIVED rollup sampled
--     from member `public_state`. Dashboard-facing only.
-- Consumer guidance mirrors the Valkey path; see
-- `scanner::flow_projector` module doc + RFC-007 §Flow Summary
-- Projection for the contract.
--
-- Additive (backward_compatible = true). No data backfill — the
-- projection is idempotent, so the next scanner tick repopulates
-- summaries for every active flow.

-- ============================================================
-- Section 1 — ff_flow_summary table (256-way HASH-partitioned)
-- ============================================================

CREATE TABLE ff_flow_summary (
    partition_key             smallint NOT NULL,
    flow_id                   uuid     NOT NULL,
    total_members             bigint   NOT NULL DEFAULT 0,
    sampled_members           integer  NOT NULL DEFAULT 0,
    members_completed         integer  NOT NULL DEFAULT 0,
    members_failed            integer  NOT NULL DEFAULT 0,
    members_cancelled         integer  NOT NULL DEFAULT 0,
    members_expired           integer  NOT NULL DEFAULT 0,
    members_skipped           integer  NOT NULL DEFAULT 0,
    members_active            integer  NOT NULL DEFAULT 0,
    members_suspended         integer  NOT NULL DEFAULT 0,
    members_waiting           integer  NOT NULL DEFAULT 0,
    members_delayed           integer  NOT NULL DEFAULT 0,
    members_rate_limited      integer  NOT NULL DEFAULT 0,
    members_waiting_children  integer  NOT NULL DEFAULT 0,
    public_flow_state         text     NOT NULL,
    last_summary_update_at    bigint   NOT NULL,
    PRIMARY KEY (partition_key, flow_id)
) PARTITION BY HASH (partition_key);

-- Section 1b — 256 HASH partitions.
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_flow_summary FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_flow_summary_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 2 — Migration annotation
-- ============================================================
INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    19,
    '0019_flow_summary',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
