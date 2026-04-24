-- RFC-v0.7 Wave 6c — timeout scanner support columns.
--
-- Additive (`backward_compatible = true`). The Wave 6c timeout
-- reconcilers (`attempt_timeout`, `lease_expiry`, `suspension_timeout`)
-- mirror the Valkey-side `ff:idx:...` ZSET scans against the
-- Postgres schema. One schema extension is needed:
--
--   * `ff_suspension_current.timeout_behavior` — the wire-string
--     encoding of `TimeoutBehavior` (`fail | cancel | expire |
--     auto_resume_with_timeout_signal | escalate`), recorded at
--     suspend time so the `suspension_timeout` reconciler can
--     apply the configured behavior without a second round-trip
--     to rehydrate policy. Nullable + NULL-treated-as-`fail` so
--     rows created by the pre-migration `suspend_impl` body keep
--     a safe default.
--
-- The attempt_timeout + lease_expiry reconcilers need no schema
-- extension — `ff_attempt.lease_expires_at_ms` already carries
-- the scan key and the partial index
-- `ff_attempt_lease_expiry_idx` already exists.

ALTER TABLE ff_suspension_current
    ADD COLUMN timeout_behavior text;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    5,
    '0005_timeout_scanners',
    (extract(epoch from clock_timestamp())*1000)::bigint,
    true
);
