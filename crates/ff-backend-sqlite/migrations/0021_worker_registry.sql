-- RFC-025 Phase 4 — SQLite worker-registry tables.
--
-- Mirrors the Postgres sibling (0021) as flat tables per RFC-023
-- §4.1 A3 (SQLite single-writer, no HASH partitioning). `lanes` is
-- stored as a sorted-joined CSV since SQLite lacks `text[]`; encoding
-- matches Valkey's CSV so downstream consumers see a consistent wire
-- shape across backends.
--
-- Body implementations land in Phase 4 (rfc-025/phase-4-sqlite); this
-- migration lands alongside Phase 3 to keep sqlite/postgres parity
-- lint green.

CREATE TABLE ff_worker_registry (
    namespace              TEXT    NOT NULL,
    worker_instance_id     TEXT    NOT NULL,
    worker_id              TEXT    NOT NULL,
    lanes                  TEXT    NOT NULL,  -- sorted-joined CSV (no text[] in SQLite)
    capabilities_csv       TEXT    NOT NULL,
    last_heartbeat_ms      INTEGER NOT NULL,
    liveness_ttl_ms        INTEGER NOT NULL,
    registered_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (namespace, worker_instance_id)
);

CREATE TABLE ff_worker_registry_event (
    namespace              TEXT    NOT NULL,
    worker_instance_id     TEXT    NOT NULL,
    event_kind             TEXT    NOT NULL,  -- 'registered' | 'heartbeat' | 'marked_dead' | 'ttl_swept'
    event_at_ms            INTEGER NOT NULL,
    reason                 TEXT    NULL,
    PRIMARY KEY (namespace, worker_instance_id, event_at_ms)
);

CREATE INDEX ff_worker_registry_ns_inst_idx
  ON ff_worker_registry (namespace, worker_instance_id);
