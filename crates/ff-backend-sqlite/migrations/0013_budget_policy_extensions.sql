-- RFC-023 Phase 1b — SQLite port of
-- `crates/ff-backend-postgres/migrations/0013_budget_policy_extensions.sql`.
--
-- Additive. SQLite's ALTER TABLE only supports one ADD COLUMN per
-- statement; the PG multi-ADD is split. Partial index shape unchanged.

ALTER TABLE ff_budget_policy
    ADD COLUMN scope_type          TEXT   NOT NULL DEFAULT '';
ALTER TABLE ff_budget_policy
    ADD COLUMN scope_id            TEXT   NOT NULL DEFAULT '';
ALTER TABLE ff_budget_policy
    ADD COLUMN enforcement_mode    TEXT   NOT NULL DEFAULT '';
ALTER TABLE ff_budget_policy
    ADD COLUMN breach_count        INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ff_budget_policy
    ADD COLUMN soft_breach_count   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ff_budget_policy
    ADD COLUMN last_breach_at_ms   INTEGER;
ALTER TABLE ff_budget_policy
    ADD COLUMN last_breach_dim     TEXT;
ALTER TABLE ff_budget_policy
    ADD COLUMN next_reset_at_ms    INTEGER;

CREATE INDEX ff_budget_policy_reset_due_idx
    ON ff_budget_policy (partition_key, next_reset_at_ms)
    WHERE next_reset_at_ms IS NOT NULL;

INSERT INTO ff_migration_annotation (version, name, applied_at_ms, backward_compatible)
VALUES (
    13,
    '0013_budget_policy_extensions',
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
