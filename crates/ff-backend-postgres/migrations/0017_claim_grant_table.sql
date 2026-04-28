-- no-transaction
-- RFC-024 PR-D — claim-grant table + lease_reclaim_count column.
--
-- Replaces the pre-RFC JSON stash at `ff_exec_core.raw_fields.claim_grant`
-- with a properly-shaped table carrying a discriminator column
-- (`kind IN ('claim','reclaim')`). Adds the `lease_reclaim_count`
-- column needed by RFC-024 §9's reclaim-cap-exceeded enforcement
-- (default ceiling 1000).
--
-- Migration numbering: RFC-024 drafts reserved 0015 for this work but
-- 0015 was not pre-applied; 0016_started_at_ms landed first. This
-- migration takes 0017. The JSON→table backfill is driven by an
-- `INSERT ... SELECT` from the pre-existing `raw_fields.claim_grant`
-- entries. The JSON field is LEFT in place for one release per
-- RFC-024 §5 / §10 (a later migration strips the residue); readers
-- post-migration consult the table only.
--
-- Additive (`backward_compatible = true`). The new table starts empty
-- except for the backfilled JSON rows; existing schedulers / workers
-- keep functioning because `raw_fields.claim_grant` remains readable.
-- The scheduler.rs read + write paths flip to the new table in this
-- PR, but a rolling-deploy window where the older process still reads
-- JSON is safe: grants issued by the new process write BOTH to the
-- table and — during a transition window — the legacy JSON blob
-- remains a valid fallback (§6 Backend parity).

-- ============================================================
-- Section 0 — pgcrypto (digest()) for deterministic grant_id backfill
-- ============================================================
-- Idempotent; stock FF environments already carry pgcrypto via operator
-- bootstrap. Declared up-front so the Section 3 backfill can call
-- `digest(text, 'sha256')` unconditionally.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ============================================================
-- Section 1 — ff_claim_grant table (256-way HASH-partitioned)
-- ============================================================

CREATE TABLE ff_claim_grant (
    partition_key       smallint NOT NULL,
    grant_id            bytea    NOT NULL,
    execution_id        uuid     NOT NULL,
    kind                text     NOT NULL CHECK (kind IN ('claim','reclaim')),
    worker_id           text     NOT NULL,
    worker_instance_id  text     NOT NULL,
    lane_id             text,
    capability_hash     text,
    worker_capabilities jsonb    NOT NULL DEFAULT '[]'::jsonb,
    route_snapshot_json text,
    admission_summary   text,
    grant_ttl_ms        bigint   NOT NULL,
    issued_at_ms        bigint   NOT NULL,
    expires_at_ms       bigint   NOT NULL,
    PRIMARY KEY (partition_key, grant_id)
) PARTITION BY HASH (partition_key);

-- Execution lookup (used by reclaim_execution to locate a kind=reclaim
-- grant by (partition_key, execution_id)).
CREATE INDEX ff_claim_grant_execution_idx
    ON ff_claim_grant (partition_key, execution_id);

-- TTL sweep index (future reaper uses this; also used by reclaim_execution
-- to reject expired grants via predicate push-down).
CREATE INDEX ff_claim_grant_expiry_idx
    ON ff_claim_grant (expires_at_ms);

-- Section 1b — 256 HASH partitions. Matches the 0001_initial convention
-- (one COMMIT per parent → avoids max_locks_per_transaction hits on
-- stock pg installs). Single parent → single DO block.
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_claim_grant FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_claim_grant_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 2 — lease_reclaim_count column on ff_exec_core
-- ============================================================
-- Set-once-per-reclaim counter. `reclaim_execution_impl` increments by
-- one on each successful reclaim; at 1000 (default) the backend
-- surfaces `ReclaimCapExceeded` and the execution transitions to
-- terminal_failed (per §4.2 impl). Existing rows default to 0.

ALTER TABLE ff_exec_core
    ADD COLUMN IF NOT EXISTS lease_reclaim_count INTEGER NOT NULL DEFAULT 0;

-- ============================================================
-- Section 3 — Backfill JSON-stashed claim grants → table rows
-- ============================================================
-- The pre-RFC scheduler at `scheduler.rs:252` stashed grants into
-- `ff_exec_core.raw_fields.claim_grant` as a JSON object:
--   {
--     "grant_key":          "pg:<hash-tag>:<uuid>:<expires_ms>:<kid>:<hex>",
--     "worker_id":          "...",
--     "worker_instance_id": "...",
--     "expires_at_ms":      <bigint>,
--     "issued_at_ms":       <bigint>,
--     "kid":                "..."
--   }
--
-- We backfill each such row as `kind='claim'`. The `grant_id` is the
-- sha256 of the stored `grant_key` string (stable, deterministic, and
-- decoupled from the HMAC encoding). `lane_id` is NULL for pre-RFC
-- fresh-claim grants (RFC asymmetry — ClaimGrant does not carry
-- lane_id; the post-rename ReclaimGrant does).

INSERT INTO ff_claim_grant (
    partition_key, grant_id, execution_id, kind,
    worker_id, worker_instance_id, lane_id,
    capability_hash, worker_capabilities,
    route_snapshot_json, admission_summary,
    grant_ttl_ms, issued_at_ms, expires_at_ms
)
SELECT
    c.partition_key,
    digest((c.raw_fields->'claim_grant'->>'grant_key')::text, 'sha256'),
    c.execution_id,
    'claim',
    c.raw_fields->'claim_grant'->>'worker_id',
    c.raw_fields->'claim_grant'->>'worker_instance_id',
    NULL,
    NULL,
    '[]'::jsonb,
    NULL,
    NULL,
    0,  -- grant_ttl_ms not persisted in legacy JSON shape
    COALESCE((c.raw_fields->'claim_grant'->>'issued_at_ms')::bigint, 0),
    COALESCE((c.raw_fields->'claim_grant'->>'expires_at_ms')::bigint, 0)
  FROM ff_exec_core c
 WHERE c.raw_fields ? 'claim_grant'
   AND (c.raw_fields->'claim_grant'->>'grant_key') IS NOT NULL
   AND (c.raw_fields->'claim_grant'->>'worker_id') IS NOT NULL
   AND (c.raw_fields->'claim_grant'->>'worker_instance_id') IS NOT NULL
ON CONFLICT (partition_key, grant_id) DO NOTHING;

-- NOTE: `digest(text, 'sha256')` requires the `pgcrypto` extension,
-- enabled in Section 0 above. Backfill is idempotent under
-- ON CONFLICT — re-running the migration on an already-seeded
-- install writes no duplicate rows.

-- ============================================================
-- Section 4 — Migration annotation
-- ============================================================
INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    17,
    '0017_claim_grant_table',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
