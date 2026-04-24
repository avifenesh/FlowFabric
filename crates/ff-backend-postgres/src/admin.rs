//! Admin / list read surface for the Postgres backend.
//!
//! **RFC-v0.7 Wave 4g.** Implements the two admin-family methods on
//! [`ff_core::engine_backend::EngineBackend`] that are read-only and
//! back operator panels (`ff-board`, CLIs):
//!
//! * [`list_lanes_impl`] — cursor-paginated enumeration of the
//!   global `ff_lane_registry` table (Q6). Lanes are seeded by
//!   `Server::start` at boot and inserted dynamically by Wave-4a's
//!   `create_execution` via `ON CONFLICT DO NOTHING`.
//! * [`list_suspended_impl`] — cursor-paginated read of
//!   `ff_suspension_current` within one partition, projecting the
//!   `reason_code` inline (issue #183) so the UI does not round-trip
//!   per row.
//!
//! `get_partition_config()` is **not** on the trait — confirmed via
//! `grep` against `crates/ff-core/src/engine_backend.rs` at Wave-4
//! assignment time. The Q5 partition-count contract is validated
//! out-of-band by `Server::start`'s boot handshake against
//! [`ff_backend_postgres::version::check_schema_version`]; admin
//! panels that want the config read `partition_config()` off the
//! backend handle (owned by [`PostgresBackend`] from construction)
//! directly — no trait method is required.
//!
//! ## SQL shape
//!
//! Both methods are single-statement reads. `list_lanes` orders by
//! `lane_id` (the primary key) for stable cursor semantics;
//! `list_suspended` orders by `(suspended_at_ms, execution_id)` to
//! match the Valkey backend's `(score, eid)` lex order — the index
//! `ff_suspension_current_suspended_at_idx` covers the `(partition_key,
//! suspended_at_ms)` prefix, with `execution_id` filtered via the
//! primary key. The cursor is opaque to callers; internally it is
//! `(suspended_at_ms, execution_id)` projected as a single
//! `ExecutionId` (the caller's sole continuation token per the trait
//! contract) + the `suspended_at_ms` is re-read on resume so the
//! keyset `(ms, eid) > (last_ms, last_eid)` works without exposing
//! the timestamp as a separate field.

use ff_core::contracts::{ListLanesPage, ListSuspendedPage, SuspendedExecutionEntry};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::PartitionKey;
use ff_core::types::{ExecutionId, LaneId};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Cursor-paginated read of the global `ff_lane_registry` table.
///
/// Lane ids are plain `text` and ordered by the primary key for stable
/// cursor semantics. `limit = 0` short-circuits before hitting the
/// database.
pub(crate) async fn list_lanes_impl(
    pool: &PgPool,
    cursor: Option<LaneId>,
    limit: usize,
) -> Result<ListLanesPage, EngineError> {
    if limit == 0 {
        return Ok(ListLanesPage::new(Vec::new(), None));
    }

    // Fetch `limit + 1` rows so we can tell whether another page
    // exists without a follow-up `COUNT(*)`. Standard keyset-paging
    // pattern.
    let fetch_n: i64 = (limit as i64).saturating_add(1);
    let cursor_str = cursor.as_ref().map(|l| l.as_str().to_owned());

    // `$1 IS NULL OR lane_id > $1` lets one query handle both the
    // "fresh start" (cursor is None) and "continue" (cursor set)
    // cases without branching on the Rust side.
    let rows = sqlx::query(
        "SELECT lane_id FROM ff_lane_registry \
         WHERE ($1::text IS NULL OR lane_id > $1) \
         ORDER BY lane_id ASC \
         LIMIT $2",
    )
    .bind(cursor_str)
    .bind(fetch_n)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let has_more = rows.len() > limit;
    let take = if has_more { limit } else { rows.len() };

    let mut lanes: Vec<LaneId> = Vec::with_capacity(take);
    for row in rows.iter().take(take) {
        let raw: String = row.get("lane_id");
        let lane = LaneId::try_new(raw.clone()).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "list_lanes: ff_lane_registry: lane_id '{raw}' is not a valid LaneId \
                 (registry corruption?): {e:?}"
            ),
        })?;
        lanes.push(lane);
    }

    let next_cursor = if has_more { lanes.last().cloned() } else { None };
    Ok(ListLanesPage::new(lanes, next_cursor))
}

/// Cursor-paginated read of suspended executions in one partition.
///
/// The cursor is an `ExecutionId` per the trait signature. On first
/// call (`cursor = None`) we start from the smallest `(suspended_at_ms,
/// execution_id)` pair in the partition. On resume we look up the
/// cursor's `suspended_at_ms` via its primary-key row and continue
/// with `(ms, eid) > (last_ms, last_eid)` keyset pagination. If the
/// cursor row has already been deleted (the execution resumed or was
/// cancelled between pages), we fall back to `execution_id > cursor`
/// ordering — lossy but forward-progressing, matching the
/// "new inserts past the cursor WILL appear in subsequent pages"
/// semantic the trait rustdoc already allows.
pub(crate) async fn list_suspended_impl(
    pool: &PgPool,
    partition: PartitionKey,
    cursor: Option<ExecutionId>,
    limit: usize,
) -> Result<ListSuspendedPage, EngineError> {
    if limit == 0 {
        return Ok(ListSuspendedPage::new(Vec::new(), None));
    }
    let parsed = partition.parse().map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("list_suspended: partition: {e}"),
    })?;
    let partition_idx: i16 = parsed.index as i16;

    // Resolve the cursor's suspended_at_ms if we have one. When the
    // cursor row no longer exists (resumed/cancelled between pages),
    // `cursor_ms` stays None and we degrade to plain `eid > cursor`.
    let (cursor_ms, cursor_uuid) = if let Some(ref c) = cursor {
        let uuid = parse_execution_uuid(c, "list_suspended")?;
        let row = sqlx::query(
            "SELECT suspended_at_ms FROM ff_suspension_current \
             WHERE partition_key = $1 AND execution_id = $2",
        )
        .bind(partition_idx)
        .bind(uuid)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;
        (row.map(|r| r.get::<i64, _>("suspended_at_ms")), Some(uuid))
    } else {
        (None, None)
    };

    let fetch_n: i64 = (limit as i64).saturating_add(1);
    // Branch on whether we have a keyset pair so the `(ms, eid)` tuple
    // comparison can use the covering index cleanly.
    let rows = match (cursor_ms, cursor_uuid) {
        (Some(last_ms), Some(last_uuid)) => {
            sqlx::query(
                "SELECT execution_id, suspended_at_ms, reason_code \
                 FROM ff_suspension_current \
                 WHERE partition_key = $1 \
                   AND (suspended_at_ms, execution_id) > ($2, $3) \
                 ORDER BY suspended_at_ms ASC, execution_id ASC \
                 LIMIT $4",
            )
            .bind(partition_idx)
            .bind(last_ms)
            .bind(last_uuid)
            .bind(fetch_n)
            .fetch_all(pool)
            .await
        }
        (None, Some(last_uuid)) => {
            // Cursor row missing — degrade to eid-only ordering.
            sqlx::query(
                "SELECT execution_id, suspended_at_ms, reason_code \
                 FROM ff_suspension_current \
                 WHERE partition_key = $1 AND execution_id > $2 \
                 ORDER BY suspended_at_ms ASC, execution_id ASC \
                 LIMIT $3",
            )
            .bind(partition_idx)
            .bind(last_uuid)
            .bind(fetch_n)
            .fetch_all(pool)
            .await
        }
        _ => {
            sqlx::query(
                "SELECT execution_id, suspended_at_ms, reason_code \
                 FROM ff_suspension_current \
                 WHERE partition_key = $1 \
                 ORDER BY suspended_at_ms ASC, execution_id ASC \
                 LIMIT $2",
            )
            .bind(partition_idx)
            .bind(fetch_n)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(map_sqlx_error)?;

    let has_more = rows.len() > limit;
    let take = if has_more { limit } else { rows.len() };

    let mut entries: Vec<SuspendedExecutionEntry> = Vec::with_capacity(take);
    for row in rows.iter().take(take) {
        let uuid: Uuid = row.get("execution_id");
        let suspended_at_ms: i64 = row.get("suspended_at_ms");
        let reason: Option<String> = row.get("reason_code");
        let eid = build_execution_id(parsed.index, uuid)?;
        entries.push(SuspendedExecutionEntry::new(
            eid,
            suspended_at_ms,
            reason.unwrap_or_default(),
        ));
    }

    let next_cursor = if has_more {
        entries.last().map(|e| e.execution_id.clone())
    } else {
        None
    };
    Ok(ListSuspendedPage::new(entries, next_cursor))
}

/// Extract the UUID half of an `ExecutionId` for a WHERE clause
/// binding. The hash-tag prefix is redundant once `partition_key` is
/// already in the predicate.
fn parse_execution_uuid(id: &ExecutionId, op: &'static str) -> Result<Uuid, EngineError> {
    let s = id.as_str();
    let rest = s.strip_prefix("{fp:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("{op}: cursor missing '{{fp:' prefix: {s}"),
    })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("{op}: cursor missing '}}:' delimiter: {s}"),
    })?;
    let uuid_str = &rest[close + 2..];
    Uuid::parse_str(uuid_str).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("{op}: cursor UUID invalid: {e}"),
    })
}

/// Rebuild an `ExecutionId` from a row's `(partition_key, execution_id)`
/// columns. `ExecutionId::parse` validates the shape so any
/// registry-level malformation surfaces as
/// [`ValidationKind::Corruption`].
fn build_execution_id(partition_idx: u16, uuid: Uuid) -> Result<ExecutionId, EngineError> {
    let s = format!("{{fp:{partition_idx}}}:{uuid}");
    ExecutionId::parse(&s).map_err(|e| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: format!(
            "list_suspended: ff_suspension_current row produced invalid ExecutionId '{s}': {e:?}"
        ),
    })
}
