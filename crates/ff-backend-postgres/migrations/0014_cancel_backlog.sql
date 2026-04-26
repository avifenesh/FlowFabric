-- no-transaction
-- RFC-020 Wave 9 Spine-A pt.2 — `ff_cancel_backlog` + `ff_cancel_backlog_member`
-- (§4.3.2).
--
-- Two partitioned tables backing `cancel_flow_header` + `ack_cancel_member`
-- (§4.2.3). §7.5 resolved to a junction-table shape over `BYTEA[]`
-- array-column because `ack_cancel_member` mutates a single element
-- and `array_remove` doesn't compose cleanly with partitioning +
-- FOR UPDATE granularity.
--
--   * `ff_cancel_backlog`         — one row per flow; carries flow-level
--     cancellation policy + reason + requested-at + optional grace
--     window + status.
--   * `ff_cancel_backlog_member`  — one row per pending member; ack'd
--     rows are DELETEd in the same tx as the parent-row
--     conditional-DELETE CTE.
--
-- Both tables are HASH-partitioned 256 ways on `partition_key`
-- matching the 0001/0002/0012 convention (flow + member executions
-- co-locate on `partition_key` via the flow's partition hash;
-- RFC-011).
--
-- Additive (`backward_compatible = true`). No destructive DDL.
--
-- Authority:
--   * rfcs/RFC-020-postgres-wave-9.md §4.3.2 + §4.2.3 + §7.5

-- ============================================================
-- Section 1 — Parent tables
-- ============================================================

-- Flow-level cancel-backlog entry. One row per flow in cancel-pending
-- state. `status='pending'` while members remain unack'd; the
-- `ack_cancel_member` conditional-DELETE CTE removes the row when the
-- last member is drained.
CREATE TABLE ff_cancel_backlog (
    partition_key        smallint NOT NULL,
    flow_id              uuid     NOT NULL,
    requested_at_ms      bigint   NOT NULL,
    requester            text     NOT NULL DEFAULT '',
    reason               text     NOT NULL DEFAULT '',
    grace_until_ms       bigint,
    cancellation_policy  text     NOT NULL,
    status               text     NOT NULL DEFAULT 'pending',
    PRIMARY KEY (partition_key, flow_id)
) PARTITION BY HASH (partition_key);

-- Junction-table row per pending member. `ack_state` flips to 'acked'
-- (or the row is DELETEd outright by the CTE); `acked_at_ms`
-- timestamps the drain for the reconciler's time-based cleanup.
CREATE TABLE ff_cancel_backlog_member (
    partition_key   smallint NOT NULL,
    flow_id         uuid     NOT NULL,
    execution_id    text     NOT NULL,
    ack_state       text     NOT NULL DEFAULT 'pending',
    acked_at_ms     bigint,
    PRIMARY KEY (partition_key, flow_id, execution_id)
) PARTITION BY HASH (partition_key);

-- ============================================================
-- Section 2 — 256-way HASH partition children
-- ============================================================
-- 2 parents × 256 children = 512 partitions. One `DO` block per parent
-- with explicit COMMIT boundaries — same lock-budget discipline as
-- 0001 §Section 7 / 0002 §Section 2 / 0012 §Section 2.

COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_cancel_backlog FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_cancel_backlog_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_cancel_backlog_member FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_cancel_backlog_member_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 3 — Secondary indexes (non-PK)
-- ============================================================
-- Pending-ack scan: the cancel-backlog reconciler polls this to
-- redrive stuck members. `WHERE ack_state = 'pending'` is the hot
-- predicate; include `flow_id` on the leading key so the scan groups
-- per-flow without a heap hit.
CREATE INDEX ff_cancel_backlog_member_pending_idx
    ON ff_cancel_backlog_member (partition_key, flow_id)
    WHERE ack_state = 'pending';

-- Time-based cleanup: prune drained members beyond the grace window.
CREATE INDEX ff_cancel_backlog_member_acked_at_idx
    ON ff_cancel_backlog_member (partition_key, acked_at_ms)
    WHERE acked_at_ms IS NOT NULL;

-- Grace-window scan (`grace_until_ms` ≤ now) for reconciler wake-up.
CREATE INDEX ff_cancel_backlog_grace_idx
    ON ff_cancel_backlog (partition_key, grace_until_ms)
    WHERE grace_until_ms IS NOT NULL;

-- ============================================================
-- Section 4 — Migration annotation (Q12)
-- ============================================================
INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    14,
    '0014_cancel_backlog',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
