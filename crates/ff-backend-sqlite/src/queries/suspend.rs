//! SQL statements for suspend + claim_resumed_execution ops
//! (RFC-023 Phase 2b.2.1).
//!
//! Mirrors `ff-backend-postgres/src/suspend_ops.rs` statement-by-
//! statement with SQLite-dialect adjustments (`jsonb → TEXT`,
//! `$n → ?n`, `FOR UPDATE` elided under single-writer `BEGIN IMMEDIATE`).

// ── Dedup replay cache (ff_suspend_dedup) ─────────────────────────────

pub const SELECT_SUSPEND_DEDUP_SQL: &str = "SELECT outcome_json FROM ff_suspend_dedup \
     WHERE partition_key = ?1 AND idempotency_key = ?2";

pub const INSERT_SUSPEND_DEDUP_SQL: &str = "INSERT INTO ff_suspend_dedup \
     (partition_key, idempotency_key, outcome_json, created_at_ms) \
     VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT DO NOTHING";

// ── Fence + attempt-index resolution ──────────────────────────────────

pub const SELECT_ATTEMPT_EPOCH_SQL: &str =
    "SELECT lease_epoch FROM ff_attempt \
      WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3";

pub const SELECT_EXEC_ATTEMPT_INDEX_SQL: &str =
    "SELECT attempt_index FROM ff_exec_core \
      WHERE partition_key = ?1 AND execution_id = ?2";

// ── ff_suspension_current row ─────────────────────────────────────────

/// Binds: 1=partition, 2=execution_id, 3=suspension_id,
///        4=suspended_at_ms, 5=timeout_at_ms, 6=reason_code,
///        7=condition_json, 8=timeout_behavior.
pub const UPSERT_SUSPENSION_CURRENT_SQL: &str = "INSERT INTO ff_suspension_current \
     (partition_key, execution_id, suspension_id, suspended_at_ms, \
      timeout_at_ms, reason_code, condition, satisfied_set, member_map, \
      timeout_behavior) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '[]', '{}', ?8) \
     ON CONFLICT (partition_key, execution_id) DO UPDATE SET \
       suspension_id = excluded.suspension_id, \
       suspended_at_ms = excluded.suspended_at_ms, \
       timeout_at_ms = excluded.timeout_at_ms, \
       reason_code = excluded.reason_code, \
       condition = excluded.condition, \
       satisfied_set = '[]', \
       member_map = '{}', \
       timeout_behavior = excluded.timeout_behavior";

pub const SELECT_SUSPENSION_CONDITION_AND_MAP_SQL: &str =
    "SELECT condition, member_map FROM ff_suspension_current \
      WHERE partition_key = ?1 AND execution_id = ?2";

pub const UPDATE_SUSPENSION_MEMBER_MAP_SQL: &str =
    "UPDATE ff_suspension_current SET member_map = ?1 \
      WHERE partition_key = ?2 AND execution_id = ?3";

// ── exec_core + attempt transitions ───────────────────────────────────

pub const UPDATE_EXEC_CORE_SUSPEND_SQL: &str = "UPDATE ff_exec_core \
     SET lifecycle_phase = 'suspended', \
         ownership_state = 'released', \
         eligibility_state = 'not_applicable', \
         public_state = 'suspended', \
         attempt_state = 'attempt_interrupted' \
     WHERE partition_key = ?1 AND execution_id = ?2";

pub const UPDATE_EXEC_CORE_RESUMABLE_SQL: &str = "UPDATE ff_exec_core \
     SET public_state = 'resumable', \
         lifecycle_phase = 'runnable', \
         eligibility_state = 'eligible_now' \
     WHERE partition_key = ?1 AND execution_id = ?2";

// #356: `started_at_ms` is set-once on ff_exec_core via migration
// 0016; the resume-claim path preserves the original first-claim
// timestamp via COALESCE (seeds `?3 = now` as a defensible fallback
// for any pre-0016-backfill exec that resumes before a first-claim
// column-write — structurally the same as the PG sibling in
// `ff-backend-postgres/src/suspend_ops.rs`).
pub const UPDATE_EXEC_CORE_RUNNING_SQL: &str = "UPDATE ff_exec_core \
     SET lifecycle_phase = 'active', ownership_state = 'leased', \
         eligibility_state = 'not_applicable', \
         public_state = 'running', attempt_state = 'running_attempt', \
         started_at_ms = COALESCE(started_at_ms, ?3) \
     WHERE partition_key = ?1 AND execution_id = ?2";

pub const SELECT_EXEC_STATE_FOR_RESUME_SQL: &str =
    "SELECT public_state, attempt_index FROM ff_exec_core \
      WHERE partition_key = ?1 AND execution_id = ?2";

pub const UPDATE_ATTEMPT_SUSPEND_SQL: &str = "UPDATE ff_attempt \
     SET worker_id = NULL, \
         worker_instance_id = NULL, \
         lease_expires_at_ms = NULL, \
         lease_epoch = lease_epoch + 1, \
         outcome = 'attempt_interrupted' \
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3";

pub const UPDATE_ATTEMPT_CLAIM_RESUMED_SQL: &str = "UPDATE ff_attempt \
     SET worker_id = ?1, worker_instance_id = ?2, \
         lease_epoch = lease_epoch + 1, \
         lease_expires_at_ms = ?3, started_at_ms = ?4, outcome = NULL \
     WHERE partition_key = ?5 AND execution_id = ?6 AND attempt_index = ?7";

pub const SELECT_ATTEMPT_LEASE_EPOCH_SQL: &str =
    "SELECT lease_epoch FROM ff_attempt \
      WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3";
