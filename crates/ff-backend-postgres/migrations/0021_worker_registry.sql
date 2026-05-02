-- RFC-025 Phase 3 — Postgres worker-registry tables.
--
-- Mirrors Valkey's `ff:worker:{ns}:{inst}:alive` + `:caps` + index set
-- behind two tables: current-state and append-only audit event log.
-- Shape follows §5.2 of rfcs/RFC-025-worker-registry.md.
--
-- `partition_key` derivation: `(fnv1a_u64(worker_instance_id.as_bytes()) % 256) as smallint`
-- — identical to the Valkey-side hashing of `worker_instance_id`, so
-- `register_worker`, `heartbeat_worker`, `mark_worker_dead`, and the
-- TTL-sweep scanner all land on the same partition for the same id.
--
-- Partial-namespace pagination + lane[] equality rely on the columns
-- named below. `lanes` is stored sorted so equality checks are stable
-- across processes writing the same registration.
CREATE TABLE ff_worker_registry (
    partition_key          smallint NOT NULL,
    namespace              text     NOT NULL,
    worker_instance_id     text     NOT NULL,
    worker_id              text     NOT NULL,
    lanes                  text[]   NOT NULL,
    capabilities_csv       text     NOT NULL,
    last_heartbeat_ms      bigint   NOT NULL,
    liveness_ttl_ms        bigint   NOT NULL,
    registered_at_ms       bigint   NOT NULL,
    PRIMARY KEY (partition_key, namespace, worker_instance_id)
) PARTITION BY HASH (partition_key);

-- 256 hash partitions, mirroring `ff_budget_usage_by_exec`
-- (migration 0020) + the 256-partition family elsewhere.
DO $$
BEGIN
  FOR i IN 0..255 LOOP
    EXECUTE format(
      'CREATE TABLE %I PARTITION OF ff_worker_registry FOR VALUES WITH (MODULUS 256, REMAINDER %s)',
      'ff_worker_registry_p' || lpad(i::text, 3, '0'),
      i
    );
  END LOOP;
END$$;

-- Append-only audit trail. Captures `registered` | `heartbeat` |
-- `marked_dead` | `ttl_swept` transitions for future operator-tooling
-- (recently-dead workers, registration bursts, TTL-driven cleanup
-- trace). Unused by this RFC's bodies except as a sink from
-- `register_worker`, `mark_worker_dead`, and the TTL-sweep scanner.
CREATE TABLE ff_worker_registry_event (
    partition_key          smallint NOT NULL,
    namespace              text     NOT NULL,
    worker_instance_id     text     NOT NULL,
    event_kind             text     NOT NULL,
    event_at_ms            bigint   NOT NULL,
    reason                 text     NULL,
    PRIMARY KEY (partition_key, namespace, worker_instance_id, event_at_ms)
) PARTITION BY HASH (partition_key);

DO $$
BEGIN
  FOR i IN 0..255 LOOP
    EXECUTE format(
      'CREATE TABLE %I PARTITION OF ff_worker_registry_event FOR VALUES WITH (MODULUS 256, REMAINDER %s)',
      'ff_worker_registry_event_p' || lpad(i::text, 3, '0'),
      i
    );
  END LOOP;
END$$;

-- `list_workers` paging cursor: ordered by `(namespace, worker_instance_id)`.
-- The PK covers `(partition_key, namespace, worker_instance_id)`, but
-- scans across all partitions (cross-namespace sweeps) still need a
-- namespace-scoped projection. This index gives us a non-partitioned
-- lookup by the `(namespace, worker_instance_id)` pair.
CREATE INDEX ff_worker_registry_ns_inst_idx
    ON ff_worker_registry (namespace, worker_instance_id);

-- Lease-expiry sweep for `list_expired_leases` uses the existing
-- `ff_attempt_lease_expiry_idx` from migration 0001 (partial index on
-- `(partition_key, lease_expires_at_ms) WHERE lease_expires_at_ms IS
-- NOT NULL`). No new index is required on `ff_attempt`.
