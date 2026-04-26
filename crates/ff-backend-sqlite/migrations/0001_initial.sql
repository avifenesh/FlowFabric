-- RFC-023 Phase 1b — SQLite-dialect port of
-- `crates/ff-backend-postgres/migrations/0001_initial.sql`.
--
-- Port rules applied (per RFC-023 §4.1):
--   * Flat tables — SQLite is single-writer, single-process; no
--     HASH-partitioning (§4.1 A3). The `partition_key INTEGER NOT
--     NULL` column stays on every table so downstream query shapes
--     match the PG schema verbatim, but there is no partition DDL.
--   * `uuid` → `BLOB` (16-byte binary form). Phase 2 query bodies
--     serialize / deserialize at the SQL boundary.
--   * `bytea` → `BLOB`. `jsonb` → `TEXT` (JSON1 `json_extract` etc.
--     on read).
--   * `text[]` for capability-routing (`required_capabilities`) is
--     replaced with a normalized junction table
--     `ff_execution_capabilities` per RFC-023 §4.1 A4. The column
--     is dropped on this side.
--   * Triggers with `pg_notify(...)` are dropped entirely —
--     broadcast moves into the Rust post-commit path per
--     RFC-023 §4.2.
--   * `CREATE INDEX CONCURRENTLY` / `USING GIN` / `INCLUDE (...)`
--     collapse to plain `CREATE INDEX` (see §4.1).
--   * `BIGSERIAL` / `GENERATED ALWAYS AS IDENTITY` → `INTEGER
--     PRIMARY KEY AUTOINCREMENT`.
--   * `extract(epoch from clock_timestamp())*1000` →
--     `CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)`.

-- ============================================================
-- Section 1 — Global tables
-- ============================================================

CREATE TABLE ff_waitpoint_hmac (
    kid           TEXT    NOT NULL PRIMARY KEY,
    secret        BLOB    NOT NULL,
    rotated_at_ms INTEGER NOT NULL,
    active        INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE ff_lane_registry (
    lane_id          TEXT    NOT NULL PRIMARY KEY,
    registered_at_ms INTEGER NOT NULL,
    registered_by    TEXT    NOT NULL
);

CREATE TABLE ff_partition_config (
    singleton_lock         INTEGER NOT NULL PRIMARY KEY DEFAULT 1
                                    CHECK (singleton_lock = 1),
    num_flow_partitions    INTEGER NOT NULL,
    num_budget_partitions  INTEGER NOT NULL,
    num_quota_partitions   INTEGER NOT NULL,
    ff_version             TEXT    NOT NULL
);

CREATE TABLE ff_migration_annotation (
    version              INTEGER NOT NULL PRIMARY KEY,
    name                 TEXT    NOT NULL,
    applied_at_ms        INTEGER NOT NULL,
    backward_compatible  INTEGER NOT NULL
);

-- ============================================================
-- Section 2 — Core execution + flow tables (flat, not partitioned)
-- ============================================================

-- `required_capabilities` lives on `ff_execution_capabilities`
-- (junction, per RFC-023 §4.1 A4) rather than an inline array.
CREATE TABLE ff_exec_core (
    partition_key         INTEGER NOT NULL,
    execution_id          BLOB    NOT NULL,
    flow_id               BLOB,
    lane_id               TEXT    NOT NULL,
    attempt_index         INTEGER NOT NULL DEFAULT 0,
    lifecycle_phase       TEXT    NOT NULL,
    ownership_state       TEXT    NOT NULL,
    eligibility_state     TEXT    NOT NULL,
    public_state          TEXT    NOT NULL,
    attempt_state         TEXT    NOT NULL,
    blocking_reason       TEXT,
    cancellation_reason   TEXT,
    cancelled_by          TEXT,
    priority              INTEGER NOT NULL DEFAULT 0,
    created_at_ms         INTEGER NOT NULL,
    terminal_at_ms        INTEGER,
    deadline_at_ms        INTEGER,
    payload               BLOB,
    result                BLOB,
    policy                TEXT,
    raw_fields            TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (partition_key, execution_id)
);

CREATE INDEX ff_exec_core_lane_phase_idx
    ON ff_exec_core (partition_key, lane_id, lifecycle_phase);

CREATE INDEX ff_exec_core_flow_idx
    ON ff_exec_core (partition_key, flow_id)
    WHERE flow_id IS NOT NULL;

CREATE INDEX ff_exec_core_deadline_idx
    ON ff_exec_core (partition_key, deadline_at_ms)
    WHERE deadline_at_ms IS NOT NULL;

-- Capability junction (RFC-023 §4.1 A4 — replaces PG text[] + GIN).
-- `WITHOUT ROWID` matches RFC-023 §4.1 A4 spec.
CREATE TABLE ff_execution_capabilities (
    execution_id BLOB NOT NULL,
    capability   TEXT NOT NULL,
    PRIMARY KEY (execution_id, capability)
) WITHOUT ROWID;

-- Reverse index for capability-first routing lookups.
CREATE INDEX ff_execution_capabilities_capability_idx
    ON ff_execution_capabilities (capability, execution_id);

CREATE TABLE ff_flow_core (
    partition_key     INTEGER NOT NULL,
    flow_id           BLOB    NOT NULL,
    graph_revision    INTEGER NOT NULL DEFAULT 0,
    public_flow_state TEXT    NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    terminal_at_ms    INTEGER,
    raw_fields        TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (partition_key, flow_id)
);

CREATE TABLE ff_attempt (
    partition_key        INTEGER NOT NULL,
    execution_id         BLOB    NOT NULL,
    attempt_index        INTEGER NOT NULL,
    worker_id            TEXT,
    worker_instance_id   TEXT,
    lease_epoch          INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms  INTEGER,
    started_at_ms        INTEGER,
    terminal_at_ms       INTEGER,
    outcome              TEXT,
    usage                TEXT,
    policy               TEXT,
    raw_fields           TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (partition_key, execution_id, attempt_index)
);

CREATE INDEX ff_attempt_lease_expiry_idx
    ON ff_attempt (partition_key, lease_expires_at_ms)
    WHERE lease_expires_at_ms IS NOT NULL;

CREATE INDEX ff_attempt_worker_idx
    ON ff_attempt (partition_key, worker_id)
    WHERE worker_id IS NOT NULL;

-- ============================================================
-- Section 3 — Flow edges + sibling-cancel (RFC-016)
-- ============================================================

CREATE TABLE ff_edge (
    partition_key  INTEGER NOT NULL,
    flow_id        BLOB    NOT NULL,
    edge_id        BLOB    NOT NULL,
    upstream_eid   BLOB    NOT NULL,
    downstream_eid BLOB    NOT NULL,
    policy         TEXT    NOT NULL,
    policy_version INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (partition_key, flow_id, edge_id)
);

-- PG uses INCLUDE(upstream_eid, policy_version). SQLite lacks
-- covering indexes; plain key-only index is equivalent semantically,
-- just forces a heap lookup for the extra cols.
CREATE INDEX ff_edge_downstream_idx
    ON ff_edge (partition_key, flow_id, downstream_eid);

CREATE INDEX ff_edge_upstream_idx
    ON ff_edge (partition_key, flow_id, upstream_eid);

-- `cancel_siblings_pending_members` is `text[]` in PG. On SQLite it
-- becomes a JSON array in a TEXT column; per-group mutations go
-- through `json_insert` / `json_remove` (Phase 2). Not a junction —
-- RFC-023 §4.1 A4 reserves the junction form for the capability
-- routing path.
CREATE TABLE ff_edge_group (
    partition_key                    INTEGER NOT NULL,
    flow_id                          BLOB    NOT NULL,
    downstream_eid                   BLOB    NOT NULL,
    policy                           TEXT    NOT NULL,
    k_target                         INTEGER NOT NULL DEFAULT 0,
    success_count                    INTEGER NOT NULL DEFAULT 0,
    fail_count                       INTEGER NOT NULL DEFAULT 0,
    skip_count                       INTEGER NOT NULL DEFAULT 0,
    running_count                    INTEGER NOT NULL DEFAULT 0,
    cancel_siblings_pending_flag     INTEGER NOT NULL DEFAULT 0,
    cancel_siblings_pending_members  TEXT    NOT NULL DEFAULT '[]',
    PRIMARY KEY (partition_key, flow_id, downstream_eid)
);

CREATE TABLE ff_pending_cancel_groups (
    partition_key   INTEGER NOT NULL,
    flow_id         BLOB    NOT NULL,
    downstream_eid  BLOB    NOT NULL,
    enqueued_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (partition_key, flow_id, downstream_eid)
);

CREATE INDEX ff_pending_cancel_groups_enqueued_idx
    ON ff_pending_cancel_groups (partition_key, enqueued_at_ms);

CREATE TABLE ff_cancel_dispatch_outbox (
    partition_key    INTEGER NOT NULL,
    flow_id          BLOB    NOT NULL,
    downstream_eid   BLOB    NOT NULL,
    target_exec_id   BLOB    NOT NULL,
    reason           TEXT    NOT NULL,
    enqueued_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (partition_key, flow_id, downstream_eid, target_exec_id)
);

CREATE INDEX ff_cancel_dispatch_outbox_enqueued_idx
    ON ff_cancel_dispatch_outbox (partition_key, enqueued_at_ms);

-- ============================================================
-- Section 4 — Suspension + waitpoints (RFC-013/014)
-- ============================================================

CREATE TABLE ff_suspension_current (
    partition_key   INTEGER NOT NULL,
    execution_id    BLOB    NOT NULL,
    suspension_id   BLOB    NOT NULL,
    suspended_at_ms INTEGER NOT NULL,
    timeout_at_ms   INTEGER,
    reason_code     TEXT,
    condition       TEXT    NOT NULL,
    satisfied_set   TEXT    NOT NULL DEFAULT '[]',
    member_map      TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (partition_key, execution_id)
);

CREATE INDEX ff_suspension_current_suspended_at_idx
    ON ff_suspension_current (partition_key, suspended_at_ms);

CREATE INDEX ff_suspension_current_timeout_idx
    ON ff_suspension_current (partition_key, timeout_at_ms)
    WHERE timeout_at_ms IS NOT NULL;

CREATE TABLE ff_waitpoint_pending (
    partition_key  INTEGER NOT NULL,
    waitpoint_id   BLOB    NOT NULL,
    execution_id   BLOB    NOT NULL,
    token_kid      TEXT    NOT NULL,
    token          TEXT    NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    expires_at_ms  INTEGER,
    condition      TEXT,
    PRIMARY KEY (partition_key, waitpoint_id)
);

CREATE INDEX ff_waitpoint_pending_expires_idx
    ON ff_waitpoint_pending (partition_key, expires_at_ms)
    WHERE expires_at_ms IS NOT NULL;

CREATE INDEX ff_waitpoint_pending_exec_idx
    ON ff_waitpoint_pending (partition_key, execution_id);

-- ============================================================
-- Section 5 — Stream tables (RFC-015)
-- ============================================================

CREATE TABLE ff_stream_frame (
    partition_key   INTEGER NOT NULL,
    execution_id    BLOB    NOT NULL,
    attempt_index   INTEGER NOT NULL,
    ts_ms           INTEGER NOT NULL,
    seq             INTEGER NOT NULL,
    fields          TEXT    NOT NULL,
    mode            TEXT    NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (partition_key, execution_id, attempt_index, ts_ms, seq)
);

-- No explicit ts/seq index: SQLite auto-indexes the PRIMARY KEY
-- above, which already matches the (partition_key, execution_id,
-- attempt_index, ts_ms, seq) scan shape used by stream catch-up.

CREATE TABLE ff_stream_summary (
    partition_key       INTEGER NOT NULL,
    execution_id        BLOB    NOT NULL,
    attempt_index       INTEGER NOT NULL,
    document_json       TEXT    NOT NULL,
    version             INTEGER NOT NULL DEFAULT 0,
    patch_kind          TEXT,
    last_updated_ms     INTEGER NOT NULL,
    first_applied_ms    INTEGER NOT NULL,
    PRIMARY KEY (partition_key, execution_id, attempt_index)
);

CREATE TABLE ff_stream_meta (
    partition_key        INTEGER NOT NULL,
    execution_id         BLOB    NOT NULL,
    attempt_index        INTEGER NOT NULL,
    ema_rate_hz          REAL    NOT NULL DEFAULT 0,
    last_append_ts_ms    INTEGER NOT NULL DEFAULT 0,
    maxlen_applied_last  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (partition_key, execution_id, attempt_index)
);

-- ============================================================
-- Section 6 — Outbox (broadcast moves to Rust per RFC-023 §4.2)
-- ============================================================
-- The AFTER-INSERT pg_notify triggers from PG 0001 §Section 8 are
-- intentionally omitted. Subscribers on SQLite wake via an in-Rust
-- post-commit channel plumbed on the backend; durable replay still
-- rides the `event_id > cursor` catch-up query against this table.
CREATE TABLE ff_completion_event (
    partition_key    INTEGER NOT NULL,
    event_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id     BLOB    NOT NULL,
    flow_id          BLOB,
    outcome          TEXT    NOT NULL,
    namespace        TEXT,
    instance_tag     TEXT,
    occurred_at_ms   INTEGER NOT NULL,
    committed_at_ms  INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER))
);

-- PG declares `(partition_key, event_id)` as the PK; SQLite requires
-- AUTOINCREMENT columns to be the sole PRIMARY KEY. A UNIQUE index
-- on the composite preserves the same lookup shape.
CREATE UNIQUE INDEX ff_completion_event_partition_event_idx
    ON ff_completion_event (partition_key, event_id);

-- No explicit index on `event_id` alone: it is `INTEGER PRIMARY KEY
-- AUTOINCREMENT`, i.e. an alias for SQLite's rowid, which is already
-- the implicit primary index. A secondary index would be pure overhead.

-- ============================================================
-- Section 7 — Migration annotation + partition-config seed
-- ============================================================

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    1,
    '0001_initial',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    0
);

-- Seed the partition-config row.
--
-- NOTE: diverges from the PG seed (256/256/256). Per RFC-023 §4.1, the
-- SQLite backend runs with scanner-supervisor fan-out N=1 — the notion
-- of hashed partition routing does not apply to a single-writer
-- embedded SQLite DB — so `num_*_partitions = 1` is the correct
-- metadata-of-record here. Phase 2+ code must NOT read these values
-- expecting PG parity; they describe the SQLite deployment shape.
--
-- `ff_version = '0.12.0'` matches the version in which this schema
-- lands (RFC-023 Phase 1b).
INSERT INTO ff_partition_config (
    singleton_lock,
    num_flow_partitions,
    num_budget_partitions,
    num_quota_partitions,
    ff_version
) VALUES (
    1, 1, 1, 1, '0.12.0'
);
