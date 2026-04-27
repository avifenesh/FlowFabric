//! SQLite dialect-forked queries for flow-staging ingress
//! (`add_execution_to_flow`, `stage_dependency_edge`,
//! `apply_dependency_to_child`).
//!
//! Populated in Phase 2b.1 per RFC-023 §4.1. Mirrors the PG reference
//! at `ff-backend-postgres/src/flow_staging.rs` statement-by-statement.

// ── add_execution_to_flow ───────────────────────────────────────────────

/// Read + lock the flow_core row. SQLite's `BEGIN IMMEDIATE` serializes
/// writers so the PG `FOR UPDATE` is unnecessary; the query shape is a
/// plain SELECT.
///
/// Returns: (public_flow_state, raw_fields).
pub(crate) const SELECT_FLOW_CORE_FOR_STAGE_SQL: &str = r#"
    SELECT public_flow_state, raw_fields
      FROM ff_flow_core
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Read the exec_core row's flow back-pointer. Returns `flow_id` as a
/// nullable `BLOB`.
pub(crate) const SELECT_EXEC_FLOW_ID_SQL: &str = r#"
    SELECT flow_id
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Stamp `exec_core.flow_id` on a successful `add_execution_to_flow`.
pub(crate) const UPDATE_EXEC_SET_FLOW_ID_SQL: &str = r#"
    UPDATE ff_exec_core
       SET flow_id = ?3
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Bump `graph_revision` + `raw_fields.node_count` on
/// `add_execution_to_flow`. Mirror of PG's `jsonb_build_object` path
/// at `ff-backend-postgres/src/flow_staging.rs:239-258`; JSON1's
/// `json_set` is the equivalent in-place mutation shape.
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. now_ms (i64)
pub(crate) const BUMP_FLOW_NODE_COUNT_SQL: &str = r#"
    UPDATE ff_flow_core
       SET graph_revision = graph_revision + 1,
           raw_fields = json_set(
               json_set(
                   raw_fields,
                   '$.node_count',
                   COALESCE(json_extract(raw_fields, '$.node_count'), 0) + 1
               ),
               '$.last_mutation_at_ms',
               ?3
           )
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Read the post-bump `node_count` so the caller can return it in
/// `AddExecutionToFlowResult::Added`.
pub(crate) const SELECT_FLOW_NODE_COUNT_SQL: &str = r#"
    SELECT COALESCE(json_extract(raw_fields, '$.node_count'), 0) AS node_count
      FROM ff_flow_core
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

// ── stage_dependency_edge ──────────────────────────────────────────────

/// CAS on `graph_revision` — succeed only if (state, rev) match and the
/// flow is open. Bump `graph_revision` + `edge_count` in the same
/// UPDATE. Returns the new revision via a follow-up SELECT (SQLite
/// `RETURNING` is available ≥3.35 but kept as a second query for
/// symmetry with the rest of this module).
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. expected_graph_revision (i64)
///   4. now_ms (i64)
pub(crate) const CAS_BUMP_FLOW_REV_SQL: &str = r#"
    UPDATE ff_flow_core
       SET graph_revision = graph_revision + 1,
           raw_fields = json_set(
               json_set(
                   raw_fields,
                   '$.edge_count',
                   COALESCE(json_extract(raw_fields, '$.edge_count'), 0) + 1
               ),
               '$.last_mutation_at_ms',
               ?4
           )
     WHERE partition_key = ?1
       AND flow_id = ?2
       AND graph_revision = ?3
       AND public_flow_state = 'open'
"#;

pub(crate) const SELECT_FLOW_REV_AND_STATE_SQL: &str = r#"
    SELECT graph_revision, public_flow_state
      FROM ff_flow_core
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Check that BOTH upstream + downstream execs belong to the given
/// flow. Returns one row per member that matches. `?3` and `?4` are
/// the two exec UUIDs.
pub(crate) const SELECT_FLOW_MEMBERSHIP_PAIR_SQL: &str = r#"
    SELECT execution_id
      FROM ff_exec_core
     WHERE partition_key = ?1
       AND flow_id = ?2
       AND (execution_id = ?3 OR execution_id = ?4)
"#;

/// Insert a staged edge row. `policy` JSON carries the immutable edge
/// record (dependency_kind / satisfaction_condition / data_passing_ref
/// / edge_state / created_at_ms / created_by / staged_at_ms /
/// applied_at_ms).
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. edge_id (Uuid)
///   4. upstream_eid (Uuid)
///   5. downstream_eid (Uuid)
///   6. policy (TEXT JSON)
pub(crate) const INSERT_EDGE_SQL: &str = r#"
    INSERT INTO ff_edge
        (partition_key, flow_id, edge_id, upstream_eid, downstream_eid,
         policy, policy_version)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
    ON CONFLICT (partition_key, flow_id, edge_id) DO NOTHING
"#;

// ── apply_dependency_to_child ──────────────────────────────────────────

/// Load the edge's policy JSON (all mutable + immutable edge state
/// rides in this column per §4.1 on-disk layout decision).
pub(crate) const SELECT_EDGE_POLICY_SQL: &str = r#"
    SELECT policy
      FROM ff_edge
     WHERE partition_key = ?1 AND flow_id = ?2 AND edge_id = ?3
"#;

/// Write back the mutated policy JSON (applied_at_ms + edge_state set
/// by the caller in Rust).
pub(crate) const UPDATE_EDGE_POLICY_SQL: &str = r#"
    UPDATE ff_edge
       SET policy = ?4
     WHERE partition_key = ?1 AND flow_id = ?2 AND edge_id = ?3
"#;

/// Upsert the edge_group aggregate — create with default AllOf when
/// missing, bump `running_count` when present. Mirrors PG at
/// `ff-backend-postgres/src/flow_staging.rs:530-546`.
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. downstream_eid (Uuid)
///   4. policy_json (TEXT JSON — default '{"kind":"all_of"}')
pub(crate) const UPSERT_EDGE_GROUP_APPLY_SQL: &str = r#"
    INSERT INTO ff_edge_group
        (partition_key, flow_id, downstream_eid, policy, running_count)
    VALUES (?1, ?2, ?3, ?4, 1)
    ON CONFLICT (partition_key, flow_id, downstream_eid) DO UPDATE
        SET running_count = ff_edge_group.running_count + 1
"#;

/// Read the post-upsert `running_count` for the caller's result payload.
pub(crate) const SELECT_EDGE_GROUP_RUNNING_COUNT_SQL: &str = r#"
    SELECT running_count
      FROM ff_edge_group
     WHERE partition_key = ?1 AND flow_id = ?2 AND downstream_eid = ?3
"#;
