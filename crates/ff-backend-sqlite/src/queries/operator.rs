//! SQL statements for Wave 9 operator-control ops (RFC-023 Phase 3.2).
//!
//! Mirrors `ff-backend-postgres/src/operator.rs`. SQLite-specific
//! translations:
//!
//! * `jsonb_set(raw_fields, '{k}', to_jsonb($::text))` →
//!   `json_set(raw_fields, '$.k', ?)`. `raw_fields` is TEXT JSON
//!   (JSON1), not JSONB.
//! * `raw_fields->>'replay_count'` → `json_extract(raw_fields,
//!   '$.replay_count')`. Cast to INTEGER happens via SQLite's implicit
//!   numeric coercion inside arithmetic.
//! * `FOR NO KEY UPDATE` / `FOR UPDATE` are no-ops — the enclosing
//!   `BEGIN IMMEDIATE` holds the RESERVED lock for the full
//!   read-modify-write window.
//! * `ExecutionId` UUIDs are bound as 16-byte `BLOB`s (see
//!   `backend::split_exec_id`), not stringified UUIDs as on PG.

// ── cancel_execution ───────────────────────────────────────────────

/// Pre-read: pin `exec_core` + current-attempt lease identity.
/// Binds: ?1 partition_key, ?2 execution_id BLOB. `attempt_index` is
/// returned by this read (not bound) and is re-bound separately on
/// the attempt-row follow-up statements.
pub(crate) const SELECT_CANCEL_PRE_SQL: &str = r#"
    SELECT ec.lifecycle_phase  AS lifecycle_phase,
           ec.public_state     AS public_state,
           ec.attempt_index    AS attempt_index,
           a.worker_instance_id AS worker_instance_id,
           a.lease_epoch       AS lease_epoch
      FROM ff_exec_core ec
      LEFT JOIN ff_attempt a
        ON a.partition_key = ec.partition_key
       AND a.execution_id  = ec.execution_id
       AND a.attempt_index = ec.attempt_index
     WHERE ec.partition_key = ?1 AND ec.execution_id = ?2
"#;

/// Flip `ff_exec_core` into the terminal `cancelled` state. Matches
/// `ff-backend-postgres/src/operator.rs:211-236`. Binds: ?1 part, ?2
/// exec_uuid BLOB, ?3 now_ms, ?4 reason, ?5 source_str.
pub(crate) const UPDATE_EXEC_CORE_CANCELLED_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase     = 'cancelled',
           ownership_state     = 'unowned',
           eligibility_state   = 'not_applicable',
           public_state        = 'cancelled',
           attempt_state       = 'cancelled',
           terminal_at_ms      = COALESCE(terminal_at_ms, ?3),
           cancellation_reason = COALESCE(cancellation_reason, ?4),
           cancelled_by        = COALESCE(cancelled_by, ?5),
           raw_fields          = json_set(raw_fields, '$.last_mutation_at', CAST(?3 AS TEXT))
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Clear the current attempt's lease fields, bump lease_epoch, mark
/// outcome='cancelled'. Binds: ?1 part, ?2 exec_uuid BLOB, ?3
/// attempt_index, ?4 now_ms.
pub(crate) const UPDATE_ATTEMPT_CANCELLED_SQL: &str = r#"
    UPDATE ff_attempt
       SET worker_instance_id   = NULL,
           lease_expires_at_ms  = NULL,
           lease_epoch          = lease_epoch + 1,
           terminal_at_ms       = ?4,
           outcome              = 'cancelled'
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

// ── revoke_lease ───────────────────────────────────────────────────

/// Pre-read: exec_core.attempt_index.
pub(crate) const SELECT_EXEC_ATTEMPT_INDEX_SQL: &str = r#"
    SELECT attempt_index
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Pre-read: attempt's current owner + epoch.
pub(crate) const SELECT_ATTEMPT_OWNER_SQL: &str = r#"
    SELECT worker_instance_id, lease_epoch
      FROM ff_attempt
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// CAS on lease_epoch — clears the lease + bumps the epoch. Row-count
/// = 0 → concurrent revoker won, surface `AlreadySatisfied
/// (epoch_moved)`. Binds: ?1 part, ?2 exec_uuid, ?3 attempt_index, ?4
/// prior_epoch.
pub(crate) const UPDATE_ATTEMPT_REVOKE_CAS_SQL: &str = r#"
    UPDATE ff_attempt
       SET worker_instance_id   = NULL,
           lease_expires_at_ms  = NULL,
           lease_epoch          = lease_epoch + 1
     WHERE partition_key = ?1
       AND execution_id  = ?2
       AND attempt_index = ?3
       AND lease_epoch   = ?4
"#;

/// Flip exec_core back to runnable (reclaimable) — gated on
/// lifecycle_phase='active' so a concurrent cancel/complete is not
/// overwritten. Binds: ?1 part, ?2 exec_uuid, ?3 now_ms.
pub(crate) const UPDATE_EXEC_CORE_RECLAIMABLE_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase   = 'runnable',
           ownership_state   = 'unowned',
           eligibility_state = 'eligible_now',
           attempt_state     = 'attempt_interrupted',
           raw_fields        = json_set(raw_fields, '$.last_mutation_at', CAST(?3 AS TEXT))
     WHERE partition_key = ?1 AND execution_id = ?2
       AND lifecycle_phase = 'active'
"#;

// ── change_priority ────────────────────────────────────────────────

/// Pre-read: gate fields + current priority. Binds: ?1 part, ?2
/// exec_uuid BLOB.
pub(crate) const SELECT_CHANGE_PRIORITY_PRE_SQL: &str = r#"
    SELECT lifecycle_phase, eligibility_state, priority
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Update priority — gated on lifecycle_phase='runnable' AND
/// eligibility_state='eligible_now' (Valkey-canonical gate from
/// `flowfabric.lua:3683-3688`). Row-count = 0 on concurrent transition
/// surfaces `ExecutionNotEligible`. Binds: ?1 part, ?2 exec_uuid, ?3
/// new_priority, ?4 now_ms.
pub(crate) const UPDATE_EXEC_CORE_PRIORITY_SQL: &str = r#"
    UPDATE ff_exec_core
       SET priority   = ?3,
           raw_fields = json_set(raw_fields, '$.last_mutation_at', CAST(?4 AS TEXT))
     WHERE partition_key = ?1 AND execution_id = ?2
       AND lifecycle_phase   = 'runnable'
       AND eligibility_state = 'eligible_now'
"#;

// ── replay_execution ───────────────────────────────────────────────

/// Pre-read: lifecycle gate + flow membership + attempt index for
/// deriving the skipped-flow-member branch.
pub(crate) const SELECT_REPLAY_PRE_SQL: &str = r#"
    SELECT lifecycle_phase, flow_id, attempt_index
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Read current attempt's outcome for the skipped-branch selector.
pub(crate) const SELECT_ATTEMPT_OUTCOME_SQL: &str = r#"
    SELECT outcome
      FROM ff_attempt
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// Reset downstream edge-group counters for the skipped-flow-member
/// replay branch. skip/fail/running → 0; success_count preserved per
/// Valkey ground-truth at `flowfabric.lua:8580`. Binds: ?1 part, ?2
/// exec_uuid_blob (downstream_eid).
pub(crate) const RESET_EDGE_GROUP_COUNTERS_SQL: &str = r#"
    UPDATE ff_edge_group
       SET skip_count    = 0,
           fail_count    = 0,
           running_count = 0
     WHERE (partition_key, flow_id, downstream_eid) IN (
       SELECT DISTINCT e.partition_key, e.flow_id, e.downstream_eid
         FROM ff_edge e
        WHERE e.partition_key   = ?1
          AND e.downstream_eid  = ?2
     )
"#;

/// Flip exec_core back to runnable + bump `replay_count`. Binds: ?1
/// part, ?2 exec_uuid BLOB, ?3 eligibility_state, ?4 public_state, ?5
/// now_ms.
///
/// SQLite's `json_set` doesn't support JSON-pointer arithmetic in a
/// single call the way PG's `jsonb_set(..., to_jsonb(... + 1))` does.
/// We nest two `json_set` calls: the inner one bumps
/// `replay_count`, the outer stamps `last_mutation_at`. The bump
/// reads the current value via `json_extract` + `COALESCE(..., 0)`
/// so the first replay initializes it from 0 → 1.
///
/// `replay_count` is stored as a JSON **number** (not a string): the
/// SQLite integer result of the arithmetic flows directly into
/// `json_set(...)` without a `CAST ... AS TEXT` wrapper, matching the
/// PG reference's `to_jsonb(... + 1)` shape. `last_mutation_at` stays
/// a JSON string to match how it is written elsewhere (e.g.
/// `build_create_execution_raw_fields`).
pub(crate) const UPDATE_EXEC_CORE_REPLAY_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase      = 'runnable',
           ownership_state      = 'unowned',
           eligibility_state    = ?3,
           public_state         = ?4,
           attempt_state        = 'pending_replay_attempt',
           terminal_at_ms       = NULL,
           result               = NULL,
           cancellation_reason  = NULL,
           cancelled_by         = NULL,
           raw_fields           = json_set(
               json_set(
                   raw_fields,
                   '$.replay_count',
                   COALESCE(CAST(json_extract(raw_fields, '$.replay_count') AS INTEGER), 0) + 1
               ),
               '$.last_mutation_at',
               CAST(?5 AS TEXT)
           )
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Reset current attempt row in-place (Rev 7 Fork 2 Option A — no new
/// attempt row). Binds: ?1 part, ?2 exec_uuid, ?3 attempt_index.
pub(crate) const UPDATE_ATTEMPT_REPLAY_RESET_SQL: &str = r#"
    UPDATE ff_attempt
       SET outcome              = NULL,
           terminal_at_ms       = NULL,
           worker_id            = NULL,
           worker_instance_id   = NULL,
           lease_expires_at_ms  = NULL,
           lease_epoch          = lease_epoch + 1
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

// ── cancel_flow_header (§4.2.3, Phase 3.3) ─────────────────────────

/// Pre-read flow core row under the write lock (`BEGIN IMMEDIATE`).
/// Binds: ?1 partition_key, ?2 flow_id BLOB.
pub(crate) const SELECT_FLOW_CORE_FOR_CANCEL_SQL: &str = r#"
    SELECT public_flow_state, raw_fields
      FROM ff_flow_core
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Flip `ff_flow_core` to cancelled + merge `cancellation_policy` +
/// `cancel_reason` into `raw_fields`. SQLite has no `jsonb ||` merge;
/// we nest two `json_set` calls. Binds: ?1 part, ?2 flow_id, ?3 now,
/// ?4 cancellation_policy, ?5 reason.
pub(crate) const UPDATE_FLOW_CORE_CANCEL_WITH_REASON_SQL: &str = r#"
    UPDATE ff_flow_core
       SET public_flow_state = 'cancelled',
           terminal_at_ms    = COALESCE(terminal_at_ms, ?3),
           raw_fields        = json_set(
                                 json_set(raw_fields,
                                          '$.cancellation_policy', ?4),
                                 '$.cancel_reason', ?5)
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Idempotent insert of the backlog header. Binds: ?1 part, ?2
/// flow_id, ?3 requested_at_ms, ?4 reason, ?5 cancellation_policy.
pub(crate) const INSERT_CANCEL_BACKLOG_SQL: &str = r#"
    INSERT INTO ff_cancel_backlog
        (partition_key, flow_id, requested_at_ms, requester, reason,
         cancellation_policy, status)
    VALUES (?1, ?2, ?3, '', ?4, ?5, 'pending')
    ON CONFLICT (partition_key, flow_id) DO NOTHING
"#;

/// Enumerate in-flight member executions for a flow. Binds: ?1 part,
/// ?2 flow_id BLOB.
pub(crate) const SELECT_FLOW_INFLIGHT_MEMBERS_SQL: &str = r#"
    SELECT execution_id
      FROM ff_exec_core
     WHERE partition_key = ?1 AND flow_id = ?2
       AND lifecycle_phase NOT IN ('terminal','cancelled')
"#;

/// Enumerate all members (used for idempotent-replay when a backlog
/// row doesn't exist yet). Binds: ?1 part, ?2 flow_id.
pub(crate) const SELECT_FLOW_ALL_MEMBERS_SQL: &str = r#"
    SELECT execution_id
      FROM ff_exec_core
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Enumerate already-enumerated backlog members (idempotent-replay
/// path). Binds: ?1 part, ?2 flow_id.
pub(crate) const SELECT_CANCEL_BACKLOG_MEMBERS_SQL: &str = r#"
    SELECT execution_id
      FROM ff_cancel_backlog_member
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Insert one backlog member row. Binds: ?1 part, ?2 flow_id, ?3
/// execution_id wire-string. SQLite has no bulk UNNEST — the caller
/// loops; under single-writer with BEGIN IMMEDIATE each statement is
/// cheap and the membership cardinality is bounded.
pub(crate) const INSERT_CANCEL_BACKLOG_MEMBER_SQL: &str = r#"
    INSERT INTO ff_cancel_backlog_member
        (partition_key, flow_id, execution_id)
    VALUES (?1, ?2, ?3)
    ON CONFLICT (partition_key, flow_id, execution_id) DO NOTHING
"#;

/// Flip one member exec_core row to cancelled for the `cancel_flow_header`
/// fan-out. Binds: ?1 part, ?2 execution_id BLOB, ?3 now, ?4 reason.
pub(crate) const UPDATE_EXEC_CORE_CANCEL_FROM_HEADER_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase     = 'cancelled',
           eligibility_state   = 'cancelled',
           public_state        = 'cancelled',
           terminal_at_ms      = COALESCE(terminal_at_ms, ?3),
           cancellation_reason = COALESCE(cancellation_reason, ?4),
           cancelled_by        = COALESCE(cancelled_by, 'cancel_flow_header')
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

// ── ack_cancel_member (§4.2.3, Phase 3.3) ──────────────────────────

/// Delete one backlog member row. Binds: ?1 part, ?2 flow_id, ?3
/// execution_id wire-string.
pub(crate) const DELETE_CANCEL_BACKLOG_MEMBER_SQL: &str = r#"
    DELETE FROM ff_cancel_backlog_member
     WHERE partition_key = ?1
       AND flow_id       = ?2
       AND execution_id  = ?3
"#;

/// Delete backlog header IFF no members remain. Binds: ?1 part, ?2
/// flow_id. Note: the NOT EXISTS subquery re-checks member_map
/// post-member-delete in the same statement window, so a last-member
/// ack drops both in a single logical step. Concurrent acks race at
/// commit time and the loser retries through `retry_serializable`.
pub(crate) const DELETE_CANCEL_BACKLOG_IF_EMPTY_SQL: &str = r#"
    DELETE FROM ff_cancel_backlog
     WHERE partition_key = ?1
       AND flow_id       = ?2
       AND NOT EXISTS (
         SELECT 1 FROM ff_cancel_backlog_member
          WHERE partition_key = ?1 AND flow_id = ?2
       )
"#;

// ── operator_event outbox (co-transactional) ───────────────────────

/// Insert one operator-event outbox row, back-filling `namespace` +
/// `instance_tag` from the co-transactional `ff_exec_core.raw_fields`
/// row (Phase 3.2 fix — mirrors the lease_event / completion_event
/// back-fill). Binds:
///
///   1. execution_id TEXT — emitted on the outbox row.
///   2. event_type TEXT.
///   3. details TEXT (nullable JSON).
///   4. occurred_at_ms (i64).
///   5. partition_key (i64) — used on both the outbox row and the
///      co-transactional exec_core lookup.
///   6. execution_id BLOB — `ff_exec_core.execution_id` is BLOB.
pub(crate) const INSERT_OPERATOR_EVENT_SQL: &str = r#"
    INSERT INTO ff_operator_event
        (execution_id, event_type, details, occurred_at_ms, partition_key,
         namespace, instance_tag)
    SELECT ?1, ?2, ?3, ?4, ?5,
           json_extract(raw_fields, '$.namespace'),
           json_extract(raw_fields, '$.tags."cairn.instance_id"')
      FROM ff_exec_core
     WHERE partition_key = ?5 AND execution_id = ?6
    UNION ALL
    SELECT ?1, ?2, ?3, ?4, ?5, NULL, NULL
     WHERE NOT EXISTS (
         SELECT 1 FROM ff_exec_core
          WHERE partition_key = ?5 AND execution_id = ?6
     )
"#;

/// Insert one operator-event outbox row, back-filling `namespace`
/// from the co-transactional `ff_flow_core.raw_fields` row. Used by
/// `flow_cancel_requested` where the `execution_id` column on the
/// outbox carries the FLOW id (Phase 3.3 parity with PG reference
/// `ff-backend-postgres/src/operator.rs:1118-1132`). `instance_tag`
/// is left NULL — flows don't carry `cairn.instance_id` tags today.
///
/// Binds:
///   1. flow_id TEXT (stringified UUID) — written on outbox row.
///   2. event_type TEXT ('flow_cancel_requested').
///   3. details TEXT (nullable JSON).
///   4. occurred_at_ms (i64).
///   5. partition_key (i64).
///   6. flow_id BLOB — for the co-transactional flow_core lookup.
pub(crate) const INSERT_OPERATOR_EVENT_FLOW_SQL: &str = r#"
    INSERT INTO ff_operator_event
        (execution_id, event_type, details, occurred_at_ms, partition_key,
         namespace, instance_tag)
    SELECT ?1, ?2, ?3, ?4, ?5,
           json_extract(raw_fields, '$.namespace'),
           NULL
      FROM ff_flow_core
     WHERE partition_key = ?5 AND flow_id = ?6
    UNION ALL
    SELECT ?1, ?2, ?3, ?4, ?5, NULL, NULL
     WHERE NOT EXISTS (
         SELECT 1 FROM ff_flow_core
          WHERE partition_key = ?5 AND flow_id = ?6
     )
"#;
