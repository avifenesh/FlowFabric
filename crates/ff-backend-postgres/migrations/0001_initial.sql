-- no-transaction
-- RFC-v0.7 Wave 3 — genesis schema for the Postgres EngineBackend.
--
-- NOTE: The leading `-- no-transaction` directive is recognised by
-- `sqlx::migrate!` and instructs the migrator NOT to wrap this file
-- in a single transaction. This is required because creating 13
-- parent × 256 child = 3 328 partitions inside one txn exceeds the
-- default `max_locks_per_transaction × max_connections` lock-slot
-- budget on stock Postgres installs. Individual statements still
-- run under pg's implicit per-statement transaction semantics; the
-- migration is *not* atomic against partial failure, but `sqlx`
-- records the applied checksum at the end so a partial apply is
-- detected and forces operator remediation (standard pg DDL
-- practice for large object-count migrations).
--
-- This migration establishes the schema-of-record for v0.7. Wave 4
-- will land the stored-proc bodies; Wave 5+ fills the trait impls.
-- This file is tables + indices + triggers only.
--
-- Authority: rfcs/drafts/v0.7-migration-master.md, with primary
-- references Q1 (LISTEN/NOTIFY outbox), Q3 (composite stream IDs),
-- Q4 (global HMAC), Q5 (HASH 256 partitioning), Q11 (isolation),
-- Q12 (operator-applied migrations + backward_compatible annotation),
-- Q14 (per-instance dual-backend — no cross-backend fields),
-- Q15 (forward-only).
--
-- Schema conventions:
--   * Every partitioned table uses PARTITION BY HASH (partition_key)
--     with 256 children. partition_key is smallint [0..255] derived
--     from the CRC16-CCITT of the relevant exec/flow/budget/quota id
--     (ff-core::partition). The hash function is Postgres' default
--     hash over smallint; pg deterministically distributes rows.
--   * Timestamps are stored as `*_ms bigint` (monotonic wall-clock
--     milliseconds) for parity with ff-core::TimestampMs.
--   * JSONB is used for raw-HGETALL-equivalent blobs and policy /
--     snapshot shapes that Wave 4 procs own.
--   * All primary keys on partitioned tables include partition_key
--     (partition-pruning requirement).
--
-- This migration is annotated `backward_compatible=false` because
-- it's the genesis schema — there is nothing older for it to be
-- compatible with. All subsequent migrations set the flag per
-- Q12's rolling-deploy rules.

-- ============================================================
-- Section 1 — Global (non-partitioned) tables
-- ============================================================

-- Q4: one-row-per-live-kid HMAC secret store. Shared across every
-- partition (unlike Valkey's per-partition replicated hash).
CREATE TABLE ff_waitpoint_hmac (
    kid           text      NOT NULL PRIMARY KEY,
    secret        bytea     NOT NULL,
    rotated_at_ms bigint    NOT NULL,
    active        boolean   NOT NULL DEFAULT true
);

-- Lane registry — backs list_lanes (Q6 adjudication).
CREATE TABLE ff_lane_registry (
    lane_id          text   NOT NULL PRIMARY KEY,
    registered_at_ms bigint NOT NULL,
    registered_by    text   NOT NULL
);

-- Singleton boot-contract config (Q5). A one-row table enforced by
-- a boolean primary key locked to TRUE. Mismatch between
-- `num_flow_partitions` and the actual child-partition count of
-- any partitioned table refuses boot per Q5.
CREATE TABLE ff_partition_config (
    singleton_lock         boolean NOT NULL PRIMARY KEY DEFAULT true
                                    CHECK (singleton_lock = true),
    num_flow_partitions    integer NOT NULL,
    num_budget_partitions  integer NOT NULL,
    num_quota_partitions   integer NOT NULL,
    ff_version             text    NOT NULL
);

-- Q12: migration ledger with backward_compatible annotation.
-- Read by `version::check_schema_version` at boot to branch the
-- rolling-deploy rules (refuse-to-start vs start-normally).
CREATE TABLE ff_migration_annotation (
    version              integer NOT NULL PRIMARY KEY,
    name                 text    NOT NULL,
    applied_at_ms        bigint  NOT NULL,
    backward_compatible  boolean NOT NULL
);

-- ============================================================
-- Section 2 — Core execution + flow tables (partitioned)
-- ============================================================

-- ff_exec_core — one row per execution; the primary auth record.
-- Mirrors the Valkey `ff:exec:{fp:N}:<eid>:core` hash. Frequently-
-- filtered columns are hoisted; everything else rides in
-- `raw_fields jsonb` so Wave-4 procs can grow fields without a DDL
-- bump. The typed columns are those that trait-method filters or
-- returns need to index / project.
CREATE TABLE ff_exec_core (
    partition_key         smallint NOT NULL,
    execution_id          uuid     NOT NULL,
    flow_id               uuid,
    lane_id               text     NOT NULL,
    required_capabilities text[]   NOT NULL DEFAULT '{}',
    attempt_index         integer  NOT NULL DEFAULT 0,
    lifecycle_phase       text     NOT NULL,
    ownership_state       text     NOT NULL,
    eligibility_state     text     NOT NULL,
    public_state          text     NOT NULL,
    attempt_state         text     NOT NULL,
    blocking_reason       text,
    cancellation_reason   text,
    cancelled_by          text,
    priority              integer  NOT NULL DEFAULT 0,
    created_at_ms         bigint   NOT NULL,
    terminal_at_ms        bigint,
    deadline_at_ms        bigint,
    payload               bytea,
    result                bytea,
    policy                jsonb,
    raw_fields            jsonb    NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (partition_key, execution_id)
) PARTITION BY HASH (partition_key);

-- Lane-eligible scan covering index (for eligibility scanners).
CREATE INDEX ff_exec_core_lane_phase_idx
    ON ff_exec_core (partition_key, lane_id, lifecycle_phase);

-- Capability-subset GIN for worker claim matching.
CREATE INDEX ff_exec_core_caps_gin_idx
    ON ff_exec_core USING GIN (required_capabilities);

-- flow_id secondary index (RFC-011 exec-flow co-location: exec +
-- flow share the same partition_key, so (partition_key, flow_id)
-- is partition-local).
CREATE INDEX ff_exec_core_flow_idx
    ON ff_exec_core (partition_key, flow_id)
    WHERE flow_id IS NOT NULL;

-- Execution-deadline scanner (replaces the Valkey ZSET
-- `execution_deadline`).
CREATE INDEX ff_exec_core_deadline_idx
    ON ff_exec_core (partition_key, deadline_at_ms)
    WHERE deadline_at_ms IS NOT NULL;

-- ff_flow_core — one row per flow. Co-located with ff_exec_core
-- via identical partition_key (RFC-011).
CREATE TABLE ff_flow_core (
    partition_key     smallint NOT NULL,
    flow_id           uuid     NOT NULL,
    graph_revision    bigint   NOT NULL DEFAULT 0,
    public_flow_state text     NOT NULL,
    created_at_ms     bigint   NOT NULL,
    terminal_at_ms    bigint,
    raw_fields        jsonb    NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (partition_key, flow_id)
) PARTITION BY HASH (partition_key);

-- ff_attempt — per-attempt lease + worker state. Keyed on
-- (execution_id, attempt_index); partition_key matches the parent
-- exec's partition_key (RFC-011).
CREATE TABLE ff_attempt (
    partition_key        smallint NOT NULL,
    execution_id         uuid     NOT NULL,
    attempt_index        integer  NOT NULL,
    worker_id            text,
    worker_instance_id   text,
    lease_epoch          bigint   NOT NULL DEFAULT 0,
    lease_expires_at_ms  bigint,
    started_at_ms        bigint,
    terminal_at_ms       bigint,
    outcome              text,
    usage                jsonb,
    policy               jsonb,
    raw_fields           jsonb    NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (partition_key, execution_id, attempt_index)
) PARTITION BY HASH (partition_key);

-- Lease-expiry scanner index (replaces Valkey `lease_expiry` ZSET).
CREATE INDEX ff_attempt_lease_expiry_idx
    ON ff_attempt (partition_key, lease_expires_at_ms)
    WHERE lease_expires_at_ms IS NOT NULL;

-- Worker-leases lookup (replaces Valkey `worker:<wid>:leases` SET).
CREATE INDEX ff_attempt_worker_idx
    ON ff_attempt (partition_key, worker_id)
    WHERE worker_id IS NOT NULL;

-- ============================================================
-- Section 3 — Flow edges + sibling-cancel (RFC-016)
-- ============================================================

-- ff_edge — directed edges on a flow graph.
CREATE TABLE ff_edge (
    partition_key  smallint NOT NULL,
    flow_id        uuid     NOT NULL,
    edge_id        uuid     NOT NULL,
    upstream_eid   uuid     NOT NULL,
    downstream_eid uuid     NOT NULL,
    policy         jsonb    NOT NULL,
    policy_version integer  NOT NULL DEFAULT 0,
    PRIMARY KEY (partition_key, flow_id, edge_id)
) PARTITION BY HASH (partition_key);

-- Dependency-resolution lookup: "what edges fan in to this
-- downstream exec?" — covering index over the policy column so the
-- scheduler can resolve without a heap hit in the hot path.
CREATE INDEX ff_edge_downstream_idx
    ON ff_edge (partition_key, flow_id, downstream_eid)
    INCLUDE (upstream_eid, policy_version);

-- Outgoing-edges lookup from an upstream exec.
CREATE INDEX ff_edge_upstream_idx
    ON ff_edge (partition_key, flow_id, upstream_eid);

-- ff_edge_group — RFC-016 downstream-per-group state: aggregate
-- success/fail/skip counters + pending-cancel bookkeeping.
CREATE TABLE ff_edge_group (
    partition_key                    smallint NOT NULL,
    flow_id                          uuid     NOT NULL,
    downstream_eid                   uuid     NOT NULL,
    policy                           jsonb    NOT NULL,
    k_target                         integer  NOT NULL DEFAULT 0,
    success_count                    integer  NOT NULL DEFAULT 0,
    fail_count                       integer  NOT NULL DEFAULT 0,
    skip_count                       integer  NOT NULL DEFAULT 0,
    running_count                    integer  NOT NULL DEFAULT 0,
    cancel_siblings_pending_flag     boolean  NOT NULL DEFAULT false,
    cancel_siblings_pending_members  text[]   NOT NULL DEFAULT '{}',
    PRIMARY KEY (partition_key, flow_id, downstream_eid)
) PARTITION BY HASH (partition_key);

-- ff_pending_cancel_groups — SET-equivalent of the Valkey
-- `pending_cancel_groups` index. Stage-C dispatcher pops rows from
-- here to dispatch sibling cancellations.
CREATE TABLE ff_pending_cancel_groups (
    partition_key   smallint NOT NULL,
    flow_id         uuid     NOT NULL,
    downstream_eid  uuid     NOT NULL,
    enqueued_at_ms  bigint   NOT NULL,
    PRIMARY KEY (partition_key, flow_id, downstream_eid)
) PARTITION BY HASH (partition_key);

CREATE INDEX ff_pending_cancel_groups_enqueued_idx
    ON ff_pending_cancel_groups (partition_key, enqueued_at_ms);

-- ff_cancel_dispatch_outbox — per-exec sibling-cancel dispatch
-- events (RFC-016 Stage C). The dispatcher polls this table and
-- fans out cancel RPCs; after successful dispatch the row is
-- deleted. Kept separate from `ff_pending_cancel_groups` because
-- the group-level entry tracks "a group has pending work" whereas
-- this outbox tracks "individual exec-IDs to cancel."
CREATE TABLE ff_cancel_dispatch_outbox (
    partition_key    smallint NOT NULL,
    flow_id          uuid     NOT NULL,
    downstream_eid   uuid     NOT NULL,
    target_exec_id   uuid     NOT NULL,
    reason           text     NOT NULL,
    enqueued_at_ms   bigint   NOT NULL,
    PRIMARY KEY (partition_key, flow_id, downstream_eid, target_exec_id)
) PARTITION BY HASH (partition_key);

CREATE INDEX ff_cancel_dispatch_outbox_enqueued_idx
    ON ff_cancel_dispatch_outbox (partition_key, enqueued_at_ms);

-- ============================================================
-- Section 4 — Suspension + waitpoints (RFC-013/014)
-- ============================================================

-- ff_suspension_current — the currently-active suspension for an
-- execution (RFC-013 + RFC-014). `satisfied_set` + `member_map`
-- carry the composite-condition evaluator state. One row per
-- suspended execution; cleared when the suspension resolves.
CREATE TABLE ff_suspension_current (
    partition_key   smallint NOT NULL,
    execution_id    uuid     NOT NULL,
    suspension_id   uuid     NOT NULL,
    suspended_at_ms bigint   NOT NULL,
    timeout_at_ms   bigint,
    reason_code     text,
    condition       jsonb    NOT NULL,
    satisfied_set   jsonb    NOT NULL DEFAULT '[]'::jsonb,
    member_map      jsonb    NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (partition_key, execution_id)
) PARTITION BY HASH (partition_key);

-- list_suspended scan index (one row per suspended exec, ordered
-- by suspend-at timestamp).
CREATE INDEX ff_suspension_current_suspended_at_idx
    ON ff_suspension_current (partition_key, suspended_at_ms);

-- Suspension-timeout scanner.
CREATE INDEX ff_suspension_current_timeout_idx
    ON ff_suspension_current (partition_key, timeout_at_ms)
    WHERE timeout_at_ms IS NOT NULL;

-- ff_waitpoint_pending — pending waitpoints awaiting signal
-- delivery. token_kid references `ff_waitpoint_hmac.kid` (global).
CREATE TABLE ff_waitpoint_pending (
    partition_key  smallint NOT NULL,
    waitpoint_id   uuid     NOT NULL,
    execution_id   uuid     NOT NULL,
    token_kid      text     NOT NULL,
    token          text     NOT NULL,
    created_at_ms  bigint   NOT NULL,
    expires_at_ms  bigint,
    condition      jsonb,
    PRIMARY KEY (partition_key, waitpoint_id)
) PARTITION BY HASH (partition_key);

-- Pending-waitpoint expiry scanner.
CREATE INDEX ff_waitpoint_pending_expires_idx
    ON ff_waitpoint_pending (partition_key, expires_at_ms)
    WHERE expires_at_ms IS NOT NULL;

-- Per-execution waitpoint lookup.
CREATE INDEX ff_waitpoint_pending_exec_idx
    ON ff_waitpoint_pending (partition_key, execution_id);

-- ============================================================
-- Section 5 — Stream tables (RFC-015; Q3 composite IDs, Q6 approx MAXLEN)
-- ============================================================

-- ff_stream_frame — append-only stream frames. IDs are the
-- composite `(ts_ms, seq)` pair (Q3 adjudication). Server mints
-- IDs on INSERT via Wave-4 stored proc (not here). The pkey
-- includes (ts_ms, seq) to make ordering authoritative and unique
-- per attempt; the local (ts_ms, seq) btree supports "frames since
-- T" queries.
CREATE TABLE ff_stream_frame (
    partition_key   smallint NOT NULL,
    execution_id    uuid     NOT NULL,
    attempt_index   integer  NOT NULL,
    ts_ms           bigint   NOT NULL,
    seq             integer  NOT NULL,
    fields          jsonb    NOT NULL,
    mode            text     NOT NULL,
    created_at_ms   bigint   NOT NULL,
    PRIMARY KEY (partition_key, execution_id, attempt_index, ts_ms, seq)
) PARTITION BY HASH (partition_key);

-- "Frames since T" + authoritative ordering index (Q3 round-1 L
-- SDK-delta-2). Combined btree on (ts_ms, seq) scoped per attempt.
CREATE INDEX ff_stream_frame_ts_seq_idx
    ON ff_stream_frame (partition_key, execution_id, attempt_index, ts_ms, seq);

-- ff_stream_summary — RFC-015 rolling summary (one row per attempt).
CREATE TABLE ff_stream_summary (
    partition_key       smallint NOT NULL,
    execution_id        uuid     NOT NULL,
    attempt_index       integer  NOT NULL,
    document_json       jsonb    NOT NULL,
    version             integer  NOT NULL DEFAULT 0,
    patch_kind          text,
    last_updated_ms     bigint   NOT NULL,
    first_applied_ms    bigint   NOT NULL,
    PRIMARY KEY (partition_key, execution_id, attempt_index)
) PARTITION BY HASH (partition_key);

-- ff_stream_meta — dynamic MAXLEN + EMA-rate state (RFC-015 §6).
-- Read by the stream-frame janitor (Q6) to decide per-attempt
-- trim cutoffs.
CREATE TABLE ff_stream_meta (
    partition_key        smallint NOT NULL,
    execution_id         uuid     NOT NULL,
    attempt_index        integer  NOT NULL,
    ema_rate_hz          double precision NOT NULL DEFAULT 0,
    last_append_ts_ms    bigint   NOT NULL DEFAULT 0,
    maxlen_applied_last  integer  NOT NULL DEFAULT 0,
    PRIMARY KEY (partition_key, execution_id, attempt_index)
) PARTITION BY HASH (partition_key);

-- ============================================================
-- Section 6 — Outbox (Q1 — LISTEN/NOTIFY wake-up + durable log)
-- ============================================================

-- ff_completion_event — Stage-1 completion outbox. Durable source
-- of truth; LISTEN/NOTIFY is only the wake signal (payload carries
-- only event_id, sidestepping the 8 KB NOTIFY payload cap per Q1).
--
-- Partitioned by HASH so consumers filtering by partition_key get
-- partition pruning. The `event_id` identity column uses a single
-- sequence shared across all partitions — pg 16 attaches the
-- identity to the parent and hands out monotonically-increasing
-- values regardless of which partition the row lands in. That
-- preserves the single-ordering invariant from Q1 (BIGSERIAL is
-- sufficient; no vector clock needed).
CREATE TABLE ff_completion_event (
    partition_key    smallint NOT NULL,
    event_id         bigint   GENERATED ALWAYS AS IDENTITY,
    execution_id     uuid     NOT NULL,
    flow_id          uuid,
    outcome          text     NOT NULL,
    namespace        text,
    instance_tag     text,
    occurred_at_ms   bigint   NOT NULL,
    committed_at_ms  bigint   NOT NULL DEFAULT (extract(epoch from clock_timestamp())*1000)::bigint,
    PRIMARY KEY (partition_key, event_id)
) PARTITION BY HASH (partition_key);

-- Replay index: consumers resume from `event_id > last_seen` per
-- Q1's cursor-recovery protocol.
CREATE INDEX ff_completion_event_event_id_idx
    ON ff_completion_event (event_id);

-- ============================================================
-- Section 7 — 256-way HASH partition children
-- ============================================================
-- pg requires every partition child to be explicitly declared;
-- pg does not auto-create HASH partitions. Each parent table
-- creates 256 children, for a total of 13 × 256 = 3 328 child
-- partitions.
--
-- Why one DO block per parent (not one master block):
-- Creating 3 328 partitions inside a single transaction needs
-- ~3 328 relation locks, which exceeds the default
-- `max_locks_per_transaction` (64) × default pool budget. By
-- splitting into 13 separate top-level DO blocks we let pg
-- commit + release locks between parents. Each block still
-- takes 256 locks — below the default threshold on a fresh
-- backend — and this keeps the migration runnable on stock
-- Postgres installs without operator pre-tuning.
--
-- Downstream note: sqlx wraps each migration file in a single
-- transaction by default, which would re-aggregate all 3 328
-- locks. This file opts out via the `-- no-transaction`
-- directive at the very top so sqlx executes each top-level
-- statement as its own implicit txn.

-- The `COMMIT; BEGIN;` separators between DO blocks are intentional
-- and load-bearing. Without `-- no-transaction` + explicit commits,
-- Postgres's simple-query protocol executes a multi-statement string
-- as one implicit transaction, which re-aggregates all 3 328 partition
-- locks. The explicit COMMIT boundaries force lock release between
-- parent tables. (psql runs each top-level statement as its own
-- implicit txn already, so this is robust across both apply paths.)

COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_exec_core FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_exec_core_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_flow_core FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_flow_core_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_attempt FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_attempt_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_edge FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_edge_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_edge_group FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_edge_group_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_pending_cancel_groups FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_pending_cancel_groups_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_cancel_dispatch_outbox FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_cancel_dispatch_outbox_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_suspension_current FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_suspension_current_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_waitpoint_pending FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_waitpoint_pending_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_stream_frame FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_stream_frame_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_stream_summary FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_stream_summary_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_stream_meta FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_stream_meta_p' || i, i); END LOOP; END $$;
COMMIT;
DO $$ BEGIN FOR i IN 0..255 LOOP EXECUTE format('CREATE TABLE %I PARTITION OF ff_completion_event FOR VALUES WITH (MODULUS 256, REMAINDER %s)', 'ff_completion_event_p' || i, i); END LOOP; END $$;
COMMIT;

-- ============================================================
-- Section 8 — LISTEN/NOTIFY triggers (Q1, Q2)
-- ============================================================

-- Completion outbox → NOTIFY 'ff_completion' with payload = event_id.
-- Triggers fire at COMMIT (pg queues NOTIFY transactionally).
CREATE FUNCTION ff_notify_completion_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('ff_completion', NEW.event_id::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_completion_event_notify_trg
    AFTER INSERT ON ff_completion_event
    FOR EACH ROW
    EXECUTE FUNCTION ff_notify_completion_event();

-- Stream-frame INSERT → per-stream NOTIFY. Channel name is
-- `ff_stream_<uuid>_<attempt_index>` (uuid 36 chars + separators +
-- attempt int fits in pg's NAMEDATALEN=63 channel-name budget).
-- Payload is the `(ts_ms, seq)` tuple so parked tailers can
-- short-circuit the cursor re-read when the frame they were
-- waiting for lands.
CREATE FUNCTION ff_notify_stream_frame() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    chan text;
BEGIN
    chan := 'ff_stream_' || NEW.execution_id::text || '_' || NEW.attempt_index::text;
    PERFORM pg_notify(chan, NEW.ts_ms::text || '-' || NEW.seq::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_stream_frame_notify_trg
    AFTER INSERT ON ff_stream_frame
    FOR EACH ROW
    EXECUTE FUNCTION ff_notify_stream_frame();

-- ============================================================
-- Section 9 — Migration annotation (Q12)
-- ============================================================

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    1,
    '0001_initial',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    false
);

-- ============================================================
-- Section 10 — Partition-config seed
-- ============================================================
-- Seeded with the v0.7 genesis values (256 across the board).
-- Operators who need a different fanout must coordinate a
-- schema-migration + data migration in a future wave.
INSERT INTO ff_partition_config (
    singleton_lock,
    num_flow_partitions,
    num_budget_partitions,
    num_quota_partitions,
    ff_version
) VALUES (
    true, 256, 256, 256, '0.7.0'
);
