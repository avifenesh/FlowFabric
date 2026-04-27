//! Budget + quota family — SQLite impl.
//!
//! **RFC-023 Phase 3.4 / RFC-020 Wave 9 Standalone-1 (Revision 6).**
//! Ports the five budget/quota admin methods + the shared
//! `report_usage_and_check_core` hot-path from
//! `ff-backend-postgres/src/budget.rs`:
//!
//!   * [`create_budget_impl`]           — §4.4.2
//!   * [`reset_budget_impl`]            — §4.4.3
//!   * [`create_quota_policy_impl`]     — §4.4.1
//!   * [`get_budget_status_impl`]       — §4.4.7
//!   * [`report_usage_admin_impl`]      — §4.4.6 admin entry
//!   * [`report_usage_impl`]            — §4.4.6 hot-path entry
//!   * [`report_usage_and_check_core`]  — shared body; maintains the
//!     0013 breach-counter columns incrementally in the same tx as
//!     the ff_budget_usage INSERTs (Valkey parity at
//!     flowfabric.lua:6576-6580,6614).
//!
//! **Lock model.** PG uses `FOR NO KEY UPDATE` on the policy row to
//! serialise the breach-UPDATE path; SQLite runs under a single-
//! writer invariant (§4.1 A3), so `BEGIN IMMEDIATE` + the
//! `retry_serializable` wrapper cover the same semantics. `reset_budget`
//! and `report_usage*` are all write transactions and wrap in the
//! retry helper for SQLITE_BUSY / SQLITE_LOCKED per §4.3.
//!
//! **`policy_json` encoding.** PG uses `jsonb`; SQLite stores it as
//! TEXT (migration 0002). The same `{hard_limits, soft_limits,
//! reset_interval_ms, on_hard_limit, on_soft_limit}` shape round-
//! trips via `serde_json::Value::to_string` / `from_str`.
//!
//! **Partition key.** SQLite is single-partition (§4.1 —
//! `num_budget_partitions = num_quota_partitions = 1` in
//! `ff_partition_config`), so the `partition_key` column is always
//! `0`. This matches the `budget_partition(..)` / `quota_partition(..)`
//! result under `PartitionConfig { num_budget_partitions: 1, ..}`,
//! and keeps the column shape identical to PG for cross-backend
//! parity tooling that greps for `partition_key = ?`.

use std::collections::{BTreeMap, HashMap};

use ff_core::backend::UsageDimensions;
use ff_core::contracts::{
    BudgetStatus, CreateBudgetArgs, CreateBudgetResult, CreateQuotaPolicyArgs,
    CreateQuotaPolicyResult, ReportUsageAdminArgs, ReportUsageResult, ResetBudgetArgs,
    ResetBudgetResult,
};
use ff_core::engine_error::{backend_context, EngineError, ValidationKind};
use ff_core::types::{BudgetId, TimestampMs};
use serde_json::{json, Value as JsonValue};
use sqlx::{Row, SqlitePool};

use crate::errors::map_sqlx_error;
use crate::queries::{budget as q_budget, quota as q_quota};
use crate::retry::retry_serializable;
use crate::tx_util::{begin_immediate, commit_or_rollback, now_ms, rollback_quiet};

/// Dedup-expiry window. Matches RFC-012 §R7.2.3's "caller-supplied
/// idempotency key" retention: 24h by default. Identical to the PG
/// constant in `ff-backend-postgres/src/budget.rs`.
const DEDUP_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// Canonical dimension-row key. Matches PG reference semantics: one
/// ff_budget_usage row per dimension, keyed by dimension name.
fn dim_row_key(name: &str) -> String {
    name.to_string()
}

/// Serialize a `ReportUsageResult` to the JSON shape stored on
/// `ff_budget_usage_dedup.outcome_json`. Same shape as PG.
fn outcome_to_json(r: &ReportUsageResult) -> JsonValue {
    match r {
        ReportUsageResult::Ok => json!({"kind": "Ok"}),
        ReportUsageResult::AlreadyApplied => json!({"kind": "AlreadyApplied"}),
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => json!({
            "kind": "SoftBreach",
            "dimension": dimension,
            "current_usage": current_usage,
            "soft_limit": soft_limit,
        }),
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => json!({
            "kind": "HardBreach",
            "dimension": dimension,
            "current_usage": current_usage,
            "hard_limit": hard_limit,
        }),
        // `#[non_exhaustive]`; future `ReportUsageResult` variants
        // reach the backend via ff-core before landing an impl here.
        // Matches the PG reference fallback (`ff-backend-postgres/src/
        // budget.rs:102`): a future variant round-trips as `Ok` on
        // dedup replay rather than surfacing as `Corruption`. This
        // is permissive-by-design for forward compatibility — the
        // tradeoff is that a consumer running newer `ff-core` against
        // an older backend binary sees `Ok` instead of the intended
        // outcome on replay. Non-replay paths use the direct result,
        // so the window is bounded to dedup-replay only.
        _ => json!({"kind": "Ok"}),
    }
}

fn outcome_from_json(v: &JsonValue) -> Result<ReportUsageResult, EngineError> {
    let kind = v.get("kind").and_then(|k| k.as_str()).ok_or_else(|| {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: "budget dedup outcome_json missing `kind`".into(),
        }
    })?;
    match kind {
        "Ok" => Ok(ReportUsageResult::Ok),
        "AlreadyApplied" => Ok(ReportUsageResult::AlreadyApplied),
        "SoftBreach" => Ok(ReportUsageResult::SoftBreach {
            dimension: v
                .get("dimension")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
            current_usage: v.get("current_usage").and_then(|d| d.as_u64()).unwrap_or(0),
            soft_limit: v.get("soft_limit").and_then(|d| d.as_u64()).unwrap_or(0),
        }),
        "HardBreach" => Ok(ReportUsageResult::HardBreach {
            dimension: v
                .get("dimension")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
            current_usage: v.get("current_usage").and_then(|d| d.as_u64()).unwrap_or(0),
            hard_limit: v.get("hard_limit").and_then(|d| d.as_u64()).unwrap_or(0),
        }),
        other => Err(EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("budget dedup outcome_json unknown kind: {other}"),
        }),
    }
}

/// Extract the dimension → limit map from a `policy_json` blob. Same
/// shape as PG.
fn limits_from_policy(policy: &JsonValue, key: &str) -> BTreeMap<String, u64> {
    policy
        .get(key)
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default()
}

/// Decode the TEXT `policy_json` column into a `serde_json::Value`.
/// Empty / invalid → empty object (no limits defined), matching the
/// "missing row / empty policy" branch in the PG reference.
fn parse_policy_text(s: &str) -> JsonValue {
    serde_json::from_str(s).unwrap_or_else(|_| JsonValue::Object(Default::default()))
}

/// Pack `CreateBudgetArgs` → `policy_json` shape.
fn build_policy_json(args: &CreateBudgetArgs) -> JsonValue {
    let mut hard = serde_json::Map::new();
    let mut soft = serde_json::Map::new();
    for (i, dim) in args.dimensions.iter().enumerate() {
        if let Some(h) = args.hard_limits.get(i).copied() {
            hard.insert(dim.clone(), json!(h));
        }
        if let Some(s) = args.soft_limits.get(i).copied() {
            soft.insert(dim.clone(), json!(s));
        }
    }
    json!({
        "hard_limits": hard,
        "soft_limits": soft,
        "reset_interval_ms": args.reset_interval_ms,
        "on_hard_limit": args.on_hard_limit,
        "on_soft_limit": args.on_soft_limit,
    })
}

// Single-partition constant — see module docs.
const PART: i64 = 0;

// ───────────────────────────────────────────────────────────────────
// §4.4.6 — shared core for `report_usage` + `report_usage_admin`
// ───────────────────────────────────────────────────────────────────

/// `EngineBackend::report_usage` — SQLite hot-path entry. Thin retry
/// wrapper around [`report_usage_and_check_core`].
pub(crate) async fn report_usage_impl(
    pool: &SqlitePool,
    budget: &BudgetId,
    dimensions: UsageDimensions,
) -> Result<ReportUsageResult, EngineError> {
    retry_serializable(|| report_usage_and_check_core(pool, budget, dimensions.clone())).await
}

/// Admin-path `report_usage_admin`. Translates the admin-shape
/// `ReportUsageAdminArgs` to `UsageDimensions` and delegates to the
/// shared core. Matches PG `report_usage_admin_impl`.
pub(crate) async fn report_usage_admin_impl(
    pool: &SqlitePool,
    budget: &BudgetId,
    args: ReportUsageAdminArgs,
) -> Result<ReportUsageResult, EngineError> {
    if args.dimensions.len() != args.deltas.len() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "report_usage_admin: dimensions and deltas length mismatch".into(),
        });
    }
    let mut custom: BTreeMap<String, u64> = BTreeMap::new();
    for (d, v) in args.dimensions.into_iter().zip(args.deltas) {
        custom.insert(d, v);
    }
    let mut ud = UsageDimensions::new();
    ud.custom = custom;
    ud.dedup_key = args.dedup_key;
    // `args.now` is dropped here — matches PG reference
    // (`ff-backend-postgres/src/budget.rs::report_usage_admin_impl`),
    // which also delegates to the core and re-derives `now_ms()`.
    // Server-clock authority is the cross-backend invariant:
    // admins cannot back-date usage writes. Reviewer flag carried
    // forward as a cross-backend design question; not a SQLite-only
    // bug to fix here.
    retry_serializable(|| report_usage_and_check_core(pool, budget, ud.clone())).await
}

/// Shared body of `report_usage` + `report_usage_admin`. Tx steps:
///
///   1. `BEGIN IMMEDIATE` (single-writer escalation).
///   2. Dedup reservation (INSERT-or-replay on
///      `ff_budget_usage_dedup`).
///   3. Load `policy_json` from `ff_budget_policy`.
///   4. Compute per-dim new values; on hard-breach increment
///      `breach_count` + `last_breach_*` and early-return.
///   5. Apply increments to `ff_budget_usage`; on soft-breach
///      increment `soft_breach_count`.
///   6. Finalise dedup row + COMMIT.
async fn report_usage_and_check_core(
    pool: &SqlitePool,
    budget: &BudgetId,
    dimensions: UsageDimensions,
) -> Result<ReportUsageResult, EngineError> {
    let budget_id_str = budget.to_string();
    let now = now_ms();

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        // ── Dedup ──
        let dedup_owned = match dimensions.dedup_key.as_deref().filter(|k| !k.is_empty()) {
            Some(dk) => {
                let inserted = sqlx::query(q_budget::INSERT_DEDUP_PLACEHOLDER_SQL)
                    .bind(PART)
                    .bind(dk)
                    .bind(now)
                    .bind(now + DEDUP_TTL_MS)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(map_sqlx_error)?;

                if inserted.is_none() {
                    let row = sqlx::query(q_budget::SELECT_DEDUP_OUTCOME_SQL)
                        .bind(PART)
                        .bind(dk)
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(map_sqlx_error)?;
                    let outcome_text: String = row.try_get("outcome_json").map_err(map_sqlx_error)?;
                    let outcome: JsonValue = serde_json::from_str(&outcome_text).map_err(|e| {
                        EngineError::Validation {
                            kind: ValidationKind::Corruption,
                            detail: format!("dedup outcome_json decode: {e}"),
                        }
                    })?;
                    // Empty placeholder ⇒ prior in-flight caller crashed
                    // before writing outcome. Treat as AlreadyApplied.
                    if outcome.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                        return Ok(ReplayOrFresh::Replay(ReportUsageResult::AlreadyApplied));
                    }
                    let parsed = outcome_from_json(&outcome)?;
                    return Ok(ReplayOrFresh::Replay(parsed));
                }
                Some(dk.to_string())
            }
            None => None,
        };

        // ── Load policy row ──
        let policy_row = sqlx::query(q_budget::SELECT_BUDGET_POLICY_ROW_SQL)
            .bind(PART)
            .bind(&budget_id_str)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let policy: JsonValue = match policy_row {
            Some(r) => {
                let text: String = r.try_get("policy_json").map_err(map_sqlx_error)?;
                parse_policy_text(&text)
            }
            // No policy row ⇒ no limits defined. Treat all reports as
            // Ok and still apply increments (Valkey parity).
            None => JsonValue::Object(Default::default()),
        };
        let hard_limits = limits_from_policy(&policy, "hard_limits");
        let soft_limits = limits_from_policy(&policy, "soft_limits");

        // ── Per-dim admission ── Hard breach on ANY dim rejects the
        //    whole report (no increments); soft breach is advisory.
        //    Sorted-key iteration keeps the reported dim deterministic.
        let mut per_dim_current: BTreeMap<String, u64> = BTreeMap::new();
        for (dim, delta) in dimensions.custom.iter() {
            let dim_key = dim_row_key(dim);

            sqlx::query(q_budget::INSERT_USAGE_ROW_SQL)
                .bind(PART)
                .bind(&budget_id_str)
                .bind(&dim_key)
                .bind(now)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

            let row = sqlx::query(q_budget::SELECT_USAGE_ROW_SQL)
                .bind(PART)
                .bind(&budget_id_str)
                .bind(&dim_key)
                .fetch_one(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
            let cur: i64 = row.try_get("current_value").map_err(map_sqlx_error)?;
            let new_val = (cur as u64).saturating_add(*delta);

            if let Some(hard) = hard_limits.get(dim)
                && *hard > 0
                && new_val > *hard
            {
                // Hard breach — no increments applied. Maintain
                // `breach_count` + `last_breach_*` (Valkey parity at
                // flowfabric.lua:6576-6580).
                sqlx::query(q_budget::UPDATE_BUDGET_HARD_BREACH_SQL)
                    .bind(now)
                    .bind(dim)
                    .bind(now)
                    .bind(PART)
                    .bind(&budget_id_str)
                    .execute(&mut *conn)
                    .await
                    .map_err(map_sqlx_error)?;

                let outcome = ReportUsageResult::HardBreach {
                    dimension: dim.clone(),
                    current_usage: cur as u64,
                    hard_limit: *hard,
                };
                if let Some(dk) = dedup_owned.as_deref() {
                    finalize_dedup(&mut conn, dk, &outcome).await?;
                }
                return Ok(ReplayOrFresh::Fresh(outcome));
            }
            per_dim_current.insert(dim.clone(), new_val);
        }

        // ── Apply increments + check soft limits. Soft is first-dim-
        //    wins (Valkey iteration-order semantics). ──
        let mut soft_breach: Option<ReportUsageResult> = None;
        for (dim, delta) in dimensions.custom.iter() {
            let dim_key = dim_row_key(dim);
            let new_val = per_dim_current[dim];

            sqlx::query(q_budget::UPDATE_USAGE_INCREMENT_SQL)
                .bind(*delta as i64)
                .bind(now)
                .bind(PART)
                .bind(&budget_id_str)
                .bind(&dim_key)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

            if soft_breach.is_none()
                && let Some(soft) = soft_limits.get(dim)
                && *soft > 0
                && new_val > *soft
            {
                soft_breach = Some(ReportUsageResult::SoftBreach {
                    dimension: dim.clone(),
                    current_usage: new_val,
                    soft_limit: *soft,
                });
            }
        }

        // Soft breach ⇒ increment soft_breach_count (Valkey parity at
        // flowfabric.lua:6614).
        if soft_breach.is_some() {
            sqlx::query(q_budget::UPDATE_BUDGET_SOFT_BREACH_SQL)
                .bind(now)
                .bind(PART)
                .bind(&budget_id_str)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        }

        let outcome = soft_breach.unwrap_or(ReportUsageResult::Ok);
        if let Some(dk) = dedup_owned.as_deref() {
            finalize_dedup(&mut conn, dk, &outcome).await?;
        }
        Ok(ReplayOrFresh::Fresh(outcome))
    }
    .await;

    match result {
        Ok(outcome) => {
            commit_or_rollback(&mut conn).await?;
            Ok(outcome.into_inner())
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// Local tag so the inner closure can distinguish "dedup replay" from
/// "fresh compute" cleanly — both commit the same tx, but replay paths
/// short-circuit the mutation chain.
enum ReplayOrFresh {
    Replay(ReportUsageResult),
    Fresh(ReportUsageResult),
}

impl ReplayOrFresh {
    fn into_inner(self) -> ReportUsageResult {
        match self {
            ReplayOrFresh::Replay(r) | ReplayOrFresh::Fresh(r) => r,
        }
    }
}

async fn finalize_dedup(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    dedup_key: &str,
    outcome: &ReportUsageResult,
) -> Result<(), EngineError> {
    let json = outcome_to_json(outcome).to_string();
    sqlx::query(q_budget::UPDATE_DEDUP_OUTCOME_SQL)
        .bind(json)
        .bind(PART)
        .bind(dedup_key)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────
// §4.4.2 — `create_budget`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn create_budget_impl(
    pool: &SqlitePool,
    args: CreateBudgetArgs,
) -> Result<CreateBudgetResult, EngineError> {
    if args.dimensions.len() != args.hard_limits.len()
        || args.dimensions.len() != args.soft_limits.len()
    {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "create_budget: dimensions / hard_limits / soft_limits length mismatch"
                .into(),
        });
    }

    retry_serializable(|| create_budget_once(pool, &args)).await
}

async fn create_budget_once(
    pool: &SqlitePool,
    args: &CreateBudgetArgs,
) -> Result<CreateBudgetResult, EngineError> {
    let budget_id = args.budget_id.clone();
    let now: i64 = args.now.0;
    let policy_json = build_policy_json(args).to_string();
    let reset_interval_ms = args.reset_interval_ms as i64;

    let row = sqlx::query(q_budget::INSERT_BUDGET_POLICY_SQL)
        .bind(PART)
        .bind(budget_id.to_string())
        .bind(policy_json)
        .bind(&args.scope_type)
        .bind(&args.scope_id)
        .bind(&args.enforcement_mode)
        // next_reset_at_ms CASE WHEN ? > 0 THEN ? + ? ELSE NULL END
        .bind(reset_interval_ms)
        .bind(now)
        .bind(reset_interval_ms)
        // created_at_ms, updated_at_ms
        .bind(now)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    Ok(match row {
        Some(_) => CreateBudgetResult::Created { budget_id },
        None => CreateBudgetResult::AlreadySatisfied { budget_id },
    })
}

// ───────────────────────────────────────────────────────────────────
// §4.4.3 — `reset_budget`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn reset_budget_impl(
    pool: &SqlitePool,
    args: ResetBudgetArgs,
) -> Result<ResetBudgetResult, EngineError> {
    retry_serializable(|| reset_budget_once(pool, &args)).await
}

async fn reset_budget_once(
    pool: &SqlitePool,
    args: &ResetBudgetArgs,
) -> Result<ResetBudgetResult, EngineError> {
    let budget_id_str = args.budget_id.to_string();
    let now: i64 = args.now.0;

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let policy_row = sqlx::query(q_budget::SELECT_BUDGET_POLICY_ROW_SQL)
            .bind(PART)
            .bind(&budget_id_str)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(policy_row) = policy_row else {
            return Err(backend_context(
                EngineError::NotFound { entity: "budget" },
                format!("reset_budget: {}", args.budget_id),
            ));
        };
        let policy_text: String = policy_row.try_get("policy_json").map_err(map_sqlx_error)?;
        let policy_json = parse_policy_text(&policy_text);
        let reset_interval_ms: i64 = policy_json
            .get("reset_interval_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        // Zero all usage rows for this budget.
        sqlx::query(q_budget::UPDATE_USAGE_ZERO_ALL_SQL)
            .bind(now)
            .bind(now)
            .bind(PART)
            .bind(&budget_id_str)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // Reset policy metadata + reschedule.
        let row = sqlx::query(q_budget::UPDATE_BUDGET_RESET_SQL)
            .bind(now)
            .bind(reset_interval_ms)
            .bind(now)
            .bind(reset_interval_ms)
            .bind(PART)
            .bind(&budget_id_str)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let next_reset: Option<i64> = row
            .try_get::<Option<i64>, _>("next_reset_at_ms")
            .map_err(map_sqlx_error)?;

        Ok(next_reset)
    }
    .await;

    match result {
        Ok(next_reset) => {
            commit_or_rollback(&mut conn).await?;
            Ok(ResetBudgetResult::Reset {
                next_reset_at: TimestampMs(next_reset.unwrap_or(0)),
            })
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// §4.4.3 — `budget_reset` reconciler entry point (RFC-023 Phase 3.5)
// ───────────────────────────────────────────────────────────────────

/// Called per row returned by the SQLite `budget_reset` reconciler
/// scan. Looks up the policy's `reset_interval_ms` from
/// `policy_json`, zeroes usage rows + clears `last_breach_*` +
/// reschedules `next_reset_at_ms`, all under a single
/// `BEGIN IMMEDIATE` txn wrapped in [`retry_serializable`] (§4.3).
///
/// Matches the PG twin `budget_reset_reconciler_apply` at
/// `ff-backend-postgres/src/budget.rs`:
///   * defensive re-read of `next_reset_at_ms` — a concurrent
///     `reset_budget` admin call may have already advanced the
///     schedule past `now`; if so this tick is a no-op.
///   * breach counters (`breach_count`, `soft_breach_count`) are
///     NOT zeroed — they are lifetime totals (PG parity +
///     `reset_budget_impl` parity).
pub(crate) async fn budget_reset_reconciler_apply(
    pool: &SqlitePool,
    budget_id: &str,
    now: i64,
) -> Result<(), EngineError> {
    retry_serializable(|| budget_reset_reconciler_once(pool, budget_id, now)).await
}

async fn budget_reset_reconciler_once(
    pool: &SqlitePool,
    budget_id: &str,
    now: i64,
) -> Result<(), EngineError> {
    let mut conn = begin_immediate(pool).await?;

    let result: Result<(), EngineError> = async {
        let policy_row = sqlx::query(
            "SELECT policy_json, next_reset_at_ms \
               FROM ff_budget_policy \
              WHERE partition_key = ? AND budget_id = ?",
        )
        .bind(PART)
        .bind(budget_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let Some(policy_row) = policy_row else {
            return Ok(());
        };

        // Defensive re-check: skip if a concurrent admin reset
        // already advanced `next_reset_at_ms` past `now`.
        let next_reset: Option<i64> = policy_row
            .try_get::<Option<i64>, _>("next_reset_at_ms")
            .map_err(map_sqlx_error)?;
        if !matches!(next_reset, Some(n) if n <= now) {
            return Ok(());
        }

        let policy_text: String = policy_row.try_get("policy_json").map_err(map_sqlx_error)?;
        let policy_json = parse_policy_text(&policy_text);
        let reset_interval_ms: i64 = policy_json
            .get("reset_interval_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        // Zero all usage rows for this budget.
        sqlx::query(q_budget::UPDATE_USAGE_ZERO_ALL_SQL)
            .bind(now)
            .bind(now)
            .bind(PART)
            .bind(budget_id)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // Reset policy metadata + reschedule. Uses the same
        // `UPDATE_BUDGET_RESET_SQL` as `reset_budget_impl` so the
        // admin + reconciler paths produce identical rows.
        sqlx::query(q_budget::UPDATE_BUDGET_RESET_SQL)
            .bind(now)
            .bind(reset_interval_ms)
            .bind(now)
            .bind(reset_interval_ms)
            .bind(PART)
            .bind(budget_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            commit_or_rollback(&mut conn).await?;
            Ok(())
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// §4.4.1 — `create_quota_policy`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn create_quota_policy_impl(
    pool: &SqlitePool,
    args: CreateQuotaPolicyArgs,
) -> Result<CreateQuotaPolicyResult, EngineError> {
    retry_serializable(|| create_quota_policy_once(pool, &args)).await
}

async fn create_quota_policy_once(
    pool: &SqlitePool,
    args: &CreateQuotaPolicyArgs,
) -> Result<CreateQuotaPolicyResult, EngineError> {
    let qid = args.quota_policy_id.clone();
    let now: i64 = args.now.0;

    let row = sqlx::query(q_quota::INSERT_QUOTA_POLICY_SQL)
        .bind(PART)
        .bind(qid.to_string())
        .bind(args.window_seconds as i64)
        .bind(args.max_requests_per_window as i64)
        .bind(args.max_concurrent as i64)
        .bind(now)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    Ok(match row {
        Some(_) => CreateQuotaPolicyResult::Created {
            quota_policy_id: qid,
        },
        None => CreateQuotaPolicyResult::AlreadySatisfied {
            quota_policy_id: qid,
        },
    })
}

// ───────────────────────────────────────────────────────────────────
// §4.4.7 — `get_budget_status`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn get_budget_status_impl(
    pool: &SqlitePool,
    budget: &BudgetId,
) -> Result<BudgetStatus, EngineError> {
    let budget_id_str = budget.to_string();

    // SELECT 1: policy row.
    let policy_row = sqlx::query(q_budget::SELECT_BUDGET_STATUS_SQL)
        .bind(PART)
        .bind(&budget_id_str)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    let Some(policy_row) = policy_row else {
        return Err(backend_context(
            EngineError::NotFound { entity: "budget" },
            format!("get_budget_status: {budget}"),
        ));
    };

    let scope_type: String = policy_row.try_get("scope_type").map_err(map_sqlx_error)?;
    let scope_id: String = policy_row.try_get("scope_id").map_err(map_sqlx_error)?;
    let enforcement_mode: String = policy_row
        .try_get("enforcement_mode")
        .map_err(map_sqlx_error)?;
    let breach_count: i64 = policy_row.try_get("breach_count").map_err(map_sqlx_error)?;
    let soft_breach_count: i64 = policy_row
        .try_get("soft_breach_count")
        .map_err(map_sqlx_error)?;
    let last_breach_at_ms: Option<i64> = policy_row
        .try_get::<Option<i64>, _>("last_breach_at_ms")
        .map_err(map_sqlx_error)?;
    let last_breach_dim: Option<String> = policy_row
        .try_get::<Option<String>, _>("last_breach_dim")
        .map_err(map_sqlx_error)?;
    let next_reset_at_ms: Option<i64> = policy_row
        .try_get::<Option<i64>, _>("next_reset_at_ms")
        .map_err(map_sqlx_error)?;
    let created_at_ms: i64 = policy_row.try_get("created_at_ms").map_err(map_sqlx_error)?;
    let policy_text: String = policy_row.try_get("policy_json").map_err(map_sqlx_error)?;
    let policy_json = parse_policy_text(&policy_text);

    // SELECT 2: usage rows.
    let usage_rows = sqlx::query(q_budget::SELECT_ALL_USAGE_ROWS_SQL)
        .bind(PART)
        .bind(&budget_id_str)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx_error)?;

    let mut usage: HashMap<String, u64> = HashMap::new();
    for r in &usage_rows {
        let key: String = r.try_get("dimensions_key").map_err(map_sqlx_error)?;
        let val: i64 = r.try_get("current_value").map_err(map_sqlx_error)?;
        if key == "_init" {
            continue;
        }
        usage.insert(key, val.max(0) as u64);
    }

    let hard_limits: HashMap<String, u64> = limits_from_policy(&policy_json, "hard_limits")
        .into_iter()
        .collect();
    let soft_limits: HashMap<String, u64> = limits_from_policy(&policy_json, "soft_limits")
        .into_iter()
        .collect();

    let fmt_opt = |v: Option<i64>| -> Option<String> { v.map(|n| n.to_string()) };

    Ok(BudgetStatus {
        budget_id: budget.to_string(),
        scope_type,
        scope_id,
        enforcement_mode,
        usage,
        hard_limits,
        soft_limits,
        breach_count: breach_count.max(0) as u64,
        soft_breach_count: soft_breach_count.max(0) as u64,
        last_breach_at: fmt_opt(last_breach_at_ms),
        last_breach_dim,
        next_reset_at: fmt_opt(next_reset_at_ms),
        created_at: Some(created_at_ms.to_string()),
    })
}
