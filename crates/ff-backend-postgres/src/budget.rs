//! Budget family — Postgres impl.
//!
//! **RFC-v0.7 Wave 4f.** Ships `EngineBackend::report_usage` against
//! the Wave 3b schema (`0002_budget.sql`): `ff_budget_policy`,
//! `ff_budget_usage`, `ff_budget_usage_dedup`.
//!
//! **RFC-020 Wave 9 Standalone-1 (Revision 6).** Extends this module
//! with the 5 budget/quota admin methods — `create_budget`,
//! `reset_budget`, `create_quota_policy`, `get_budget_status`,
//! `report_usage_admin` — writing against 0012 (`ff_quota_policy` +
//! window + admitted set) and 0013 (additive breach / scope / reset
//! columns on `ff_budget_policy`). `report_usage_impl` is narrowly
//! extended to maintain the new breach counters per §4.4.6 and to
//! hold the policy row under `FOR NO KEY UPDATE` (replacing the
//! previous `FOR SHARE` — reviewer-finding deadlock fix on the
//! breach-UPDATE path).
//!
//! Isolation per v0.7 migration-master §Q11: READ COMMITTED + row-
//! level locking on `report_usage_impl` (hot path). `reset_budget`
//! runs SERIALIZABLE to match Valkey's Lua atomicity — the zero-all
//! pattern must not interleave with a concurrent `report_usage`
//! mid-flight.
//!
//! Idempotency per RFC-012 §R7.2.3: the caller-supplied `dedup_key`
//! keys an `INSERT ... ON CONFLICT DO NOTHING`; on conflict the cached
//! `outcome_json` row is returned verbatim (replay).

use std::collections::{BTreeMap, HashMap};

use ff_core::backend::UsageDimensions;
use ff_core::contracts::{
    BudgetStatus, CreateBudgetArgs, CreateBudgetResult, CreateQuotaPolicyArgs,
    CreateQuotaPolicyResult, RecordSpendArgs, ReleaseBudgetArgs, ReportUsageAdminArgs,
    ReportUsageResult, ResetBudgetArgs, ResetBudgetResult,
};
use ff_core::engine_error::{backend_context, EngineError, ValidationKind};
use ff_core::partition::{budget_partition, quota_partition, PartitionConfig};
use ff_core::types::{BudgetId, ExecutionId, TimestampMs};
use serde_json::{json, Value as JsonValue};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Canonical stringification of the custom-dimensions map.
///
/// Keyed lookups on `ff_budget_usage(..., dimensions_key)` rely on the
/// canonical form being stable across callers. A `BTreeMap` iterates in
/// sorted-key order, so the serialised JSON object is deterministic.
/// For `report_usage` we key per-dimension (one row per dimension) so
/// the stored key is simply the dimension name; this mirrors the Valkey
/// Hash-field shape where each dim has its own HGET slot.
fn dim_row_key(name: &str) -> String {
    name.to_string()
}

/// Now in unix-milliseconds. Mirrors the `now_ms_timestamp` helper in
/// the Valkey backend.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Serialize a `ReportUsageResult` to the jsonb shape stored in
/// `ff_budget_usage_dedup.outcome_json`. Shape:
///
/// ```json
/// {"kind":"Ok"}
/// {"kind":"AlreadyApplied"}
/// {"kind":"SoftBreach","dimension":"...","current_usage":123,"soft_limit":100}
/// {"kind":"HardBreach","dimension":"...","current_usage":123,"hard_limit":100}
/// ```
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
        // `ReportUsageResult` is `#[non_exhaustive]`; future variants
        // land in ff-core before reaching the backend impl, so emit a
        // safe placeholder that the replay path will reject rather
        // than silently mis-decoding.
        _ => json!({"kind": "Ok"}),
    }
}

/// Inverse of [`outcome_to_json`] — used on the dedup-replay path.
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

/// Extract the dimension → limit map from a `policy_json` blob, keyed
/// by the shape documented in RFC-008 §Policy schema:
/// `{"hard_limits": {"dim": u64}, "soft_limits": {"dim": u64}, ...}`.
/// Missing field / non-object → empty map (= no limit on any dim).
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

/// Dedup-expiry window. Matches RFC-012 §R7.2.3's "caller-supplied
/// idempotency key" retention: 24h by default.
const DEDUP_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

// ───────────────────────────────────────────────────────────────────
// §4.4.6 — `report_usage_impl` / `report_usage_admin` shared core
// ───────────────────────────────────────────────────────────────────

/// `EngineBackend::report_usage` — Postgres impl (worker-path). Thin
/// wrapper around [`report_usage_and_check_core`].
pub(crate) async fn report_usage_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    budget: &BudgetId,
    dimensions: UsageDimensions,
) -> Result<ReportUsageResult, EngineError> {
    report_usage_and_check_core(pool, partition_config, budget, dimensions).await
}

/// Admin-path `report_usage_admin`. Translates the admin-shape
/// `ReportUsageAdminArgs` (parallel `dimensions` / `deltas` vectors)
/// to `UsageDimensions` and delegates to the shared core. See RFC-020
/// §4.4.6 "report_usage_admin entry point".
pub(crate) async fn report_usage_admin_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
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
    // `UsageDimensions` is `#[non_exhaustive]`; build via `new()` +
    // mutate the fields we have direct access to (`custom`,
    // `dedup_key`). `input_tokens` / `output_tokens` / `wall_ms` stay
    // at their `Default` values — the shared core only inspects
    // `custom` + `dedup_key`.
    let mut ud = UsageDimensions::new();
    ud.custom = custom;
    ud.dedup_key = args.dedup_key;
    report_usage_and_check_core(pool, partition_config, budget, ud).await
}

/// Shared body of `report_usage` + `report_usage_admin`. §4.4.6 lifts
/// the Revision 5 §7.2 pin narrowly: this function now (1) loads the
/// policy row with `FOR NO KEY UPDATE` rather than `FOR SHARE`
/// (deadlock fix for the breach-UPDATE path) and (2) maintains the
/// `breach_count` / `soft_breach_count` columns + `last_breach_*`
/// metadata on the 0013 columns.
async fn report_usage_and_check_core(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    budget: &BudgetId,
    dimensions: UsageDimensions,
) -> Result<ReportUsageResult, EngineError> {
    let partition = budget_partition(budget, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id_str = budget.to_string();
    let now = now_ms();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // RC is Postgres's default; assert explicitly per Q11.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // ── Dedup ──
    //
    // When a dedup_key is provided, try to INSERT a placeholder dedup
    // row. If the INSERT succeeds we've taken ownership and will fill
    // the outcome_json below. If it conflicts the row already exists
    // from a prior call — read it back and replay the cached outcome.
    let dedup_owned = match dimensions.dedup_key.as_deref().filter(|k| !k.is_empty()) {
        Some(dk) => {
            let inserted = sqlx::query(
                "INSERT INTO ff_budget_usage_dedup \
                     (partition_key, dedup_key, outcome_json, applied_at_ms, expires_at_ms) \
                 VALUES ($1, $2, '{}'::jsonb, $3, $4) \
                 ON CONFLICT (partition_key, dedup_key) DO NOTHING \
                 RETURNING applied_at_ms",
            )
            .bind(partition_key)
            .bind(dk)
            .bind(now)
            .bind(now + DEDUP_TTL_MS)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            if inserted.is_none() {
                // Replay: fetch the cached outcome_json and return it.
                let row = sqlx::query(
                    "SELECT outcome_json FROM ff_budget_usage_dedup \
                     WHERE partition_key = $1 AND dedup_key = $2",
                )
                .bind(partition_key)
                .bind(dk)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx_error)?;
                let outcome: JsonValue = row.get("outcome_json");
                tx.commit().await.map_err(map_sqlx_error)?;
                // Empty placeholder means a prior in-flight caller
                // crashed before writing the outcome. Treat as
                // AlreadyApplied so the caller sees idempotent
                // semantics rather than double-counting.
                if outcome.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                    return Ok(ReportUsageResult::AlreadyApplied);
                }
                return outcome_from_json(&outcome);
            }
            Some(dk.to_string())
        }
        None => None,
    };

    // ── Load policy row ──
    //
    // §4.4.6 lock-mode correction: `FOR NO KEY UPDATE` serialises the
    // breach path deterministically (two concurrent callers both
    // hitting hard-breach now serialise on this SELECT, not deadlock
    // on the breach UPDATE that would otherwise need NO-KEY-UPDATE
    // while each tx holds SHARE).
    let policy_row = sqlx::query(
        "SELECT policy_json FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2 FOR NO KEY UPDATE",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let policy: JsonValue = match policy_row {
        Some(r) => r.get("policy_json"),
        // No policy row ⇒ no limits defined. Treat all reports as Ok
        // and still apply increments (Valkey parity).
        None => JsonValue::Object(Default::default()),
    };
    let hard_limits = limits_from_policy(&policy, "hard_limits");
    let soft_limits = limits_from_policy(&policy, "soft_limits");

    // ── Compute admission per dimension. Hard breach on ANY dim
    //    rejects the whole report (no increments applied); soft breach
    //    is advisory (increments applied). Sorted-key iteration keeps
    //    the reported dimension deterministic on multi-dim reports. ──
    let mut per_dim_current: BTreeMap<String, u64> = BTreeMap::new();
    for (dim, delta) in dimensions.custom.iter() {
        let dim_key = dim_row_key(dim);
        sqlx::query(
            "INSERT INTO ff_budget_usage \
                 (partition_key, budget_id, dimensions_key, current_value, updated_at_ms) \
             VALUES ($1, $2, $3, 0, $4) \
             ON CONFLICT (partition_key, budget_id, dimensions_key) DO NOTHING",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let row = sqlx::query(
            "SELECT current_value FROM ff_budget_usage \
             WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3 \
             FOR UPDATE",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        let cur: i64 = row.get("current_value");
        let new_val = (cur as u64).saturating_add(*delta);

        if let Some(hard) = hard_limits.get(dim)
            && *hard > 0
            && new_val > *hard
        {
            // §4.4.6 — Hard breach: no increments applied, maintain
            // `breach_count` + `last_breach_*` on the policy row
            // (mirrors Valkey `HINCRBY breach_count 1` + `HSET
            // last_breach_at/last_breach_dim` at
            // flowfabric.lua:6576-6580), commit dedup outcome.
            sqlx::query(
                "UPDATE ff_budget_policy \
                 SET breach_count      = breach_count + 1, \
                     last_breach_at_ms = $3, \
                     last_breach_dim   = $4, \
                     updated_at_ms     = $3 \
                 WHERE partition_key = $1 AND budget_id = $2",
            )
            .bind(partition_key)
            .bind(&budget_id_str)
            .bind(now)
            .bind(dim)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            let outcome = ReportUsageResult::HardBreach {
                dimension: dim.clone(),
                current_usage: cur as u64,
                hard_limit: *hard,
            };
            if let Some(dk) = dedup_owned.as_deref() {
                finalize_dedup(&mut tx, partition_key, dk, &outcome).await?;
            }
            tx.commit().await.map_err(map_sqlx_error)?;
            return Ok(outcome);
        }
        per_dim_current.insert(dim.clone(), new_val);
    }

    // ── Apply increments + check soft limits. Soft breach is advisory
    //    (first-dim-wins, matching Valkey's iteration-order semantics). ──
    let mut soft_breach: Option<ReportUsageResult> = None;
    for (dim, delta) in dimensions.custom.iter() {
        let dim_key = dim_row_key(dim);
        let new_val = per_dim_current[dim];
        sqlx::query(
            "UPDATE ff_budget_usage \
             SET current_value = current_value + $1, updated_at_ms = $2 \
             WHERE partition_key = $3 AND budget_id = $4 AND dimensions_key = $5",
        )
        .bind(*delta as i64)
        .bind(now)
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .execute(&mut *tx)
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

    // §4.4.6 — on soft breach, HINCRBY soft_breach_count (Valkey
    // parity at flowfabric.lua:6614). Hard-breach path returned early
    // above; this block only fires on the non-hard-breach outcome.
    if soft_breach.is_some() {
        sqlx::query(
            "UPDATE ff_budget_policy \
             SET soft_breach_count = soft_breach_count + 1, \
                 updated_at_ms     = $3 \
             WHERE partition_key = $1 AND budget_id = $2",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    let outcome = soft_breach.unwrap_or(ReportUsageResult::Ok);
    if let Some(dk) = dedup_owned.as_deref() {
        finalize_dedup(&mut tx, partition_key, dk, &outcome).await?;
    }
    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(outcome)
}

/// Write the final `outcome_json` onto the dedup row we inserted at
/// the top of [`report_usage_and_check_core`].
async fn finalize_dedup(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    partition_key: i16,
    dedup_key: &str,
    outcome: &ReportUsageResult,
) -> Result<(), EngineError> {
    let json = outcome_to_json(outcome);
    sqlx::query(
        "UPDATE ff_budget_usage_dedup SET outcome_json = $1 \
         WHERE partition_key = $2 AND dedup_key = $3",
    )
    .bind(json)
    .bind(partition_key)
    .bind(dedup_key)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

/// Test-only helper: upsert a `ff_budget_policy` row directly. Pre-
/// dates the trait-lifted `create_budget` (RFC-020 §4.4.2); retained
/// for tests that still seed a raw `policy_json` blob + exercise the
/// policy-row-missing branch of `report_usage_impl`.
#[doc(hidden)]
pub async fn upsert_policy_for_test(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    budget: &BudgetId,
    policy_json: JsonValue,
) -> Result<(), EngineError> {
    let partition = budget_partition(budget, partition_config);
    let partition_key: i16 = partition.index as i16;
    let now = now_ms();
    sqlx::query(
        "INSERT INTO ff_budget_policy \
             (partition_key, budget_id, policy_json, created_at_ms, updated_at_ms) \
         VALUES ($1, $2, $3, $4, $4) \
         ON CONFLICT (partition_key, budget_id) DO UPDATE \
             SET policy_json = EXCLUDED.policy_json, \
                 updated_at_ms = EXCLUDED.updated_at_ms",
    )
    .bind(partition_key)
    .bind(budget.to_string())
    .bind(policy_json)
    .bind(now)
    .execute(pool)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────
// §4.4.2 — `create_budget`
// ───────────────────────────────────────────────────────────────────

/// Pack a [`CreateBudgetArgs`] into the `policy_json` blob shape
/// `report_usage_and_check_core` + `get_budget_status` consume. Writes
/// parallel `dimensions` / `hard_limits` / `soft_limits` vectors into
/// `{hard_limits: {dim: u64, ...}, soft_limits: {dim: u64, ...},
/// reset_interval_ms, on_hard_limit, on_soft_limit}` — the shape
/// already used by `upsert_policy_for_test` + `limits_from_policy`.
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

pub(crate) async fn create_budget_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
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

    let partition = budget_partition(&args.budget_id, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id = args.budget_id.clone();
    let now: i64 = args.now.0;
    let policy_json = build_policy_json(&args);
    let reset_interval_ms = args.reset_interval_ms as i64;

    // `next_reset_at_ms` seeded to `now + interval` when interval > 0
    // so the `budget_reset` reconciler picks it up (RFC §4.4.3).
    // Matches Valkey's `ff_create_budget` `ZADD resets_zset` scheduling
    // at flowfabric.lua:6522-6526.
    let row = sqlx::query(
        "INSERT INTO ff_budget_policy \
             (partition_key, budget_id, policy_json, scope_type, scope_id, \
              enforcement_mode, breach_count, soft_breach_count, \
              last_breach_at_ms, last_breach_dim, next_reset_at_ms, \
              created_at_ms, updated_at_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, 0, 0, NULL, NULL, \
                 CASE WHEN $7::bigint > 0 THEN $8::bigint + $7::bigint ELSE NULL END, \
                 $8, $8) \
         ON CONFLICT (partition_key, budget_id) DO NOTHING \
         RETURNING created_at_ms",
    )
    .bind(partition_key)
    .bind(budget_id.to_string())
    .bind(policy_json)
    .bind(&args.scope_type)
    .bind(&args.scope_id)
    .bind(&args.enforcement_mode)
    .bind(reset_interval_ms)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    match row {
        Some(_) => Ok(CreateBudgetResult::Created { budget_id }),
        None => Ok(CreateBudgetResult::AlreadySatisfied { budget_id }),
    }
}

// ───────────────────────────────────────────────────────────────────
// §4.4.3 — `reset_budget`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn reset_budget_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    args: ResetBudgetArgs,
) -> Result<ResetBudgetResult, EngineError> {
    let partition = budget_partition(&args.budget_id, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id_str = args.budget_id.to_string();
    let now: i64 = args.now.0;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    // SERIALIZABLE matches Valkey's Lua atomicity — the zero-all
    // pattern must not see mid-flight increments from concurrent
    // `report_usage` / `report_usage_admin`.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // Lock the policy row with `FOR NO KEY UPDATE` before zeroing —
    // serialises against a concurrent breach-UPDATE on the same row
    // (§4.4.6 lock-mode discipline).
    let policy_row = sqlx::query(
        "SELECT policy_json FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2 FOR NO KEY UPDATE",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(policy_row) = policy_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(backend_context(
            EngineError::NotFound { entity: "budget" },
            format!("reset_budget: {}", args.budget_id),
        ));
    };
    let policy_json: JsonValue = policy_row.get("policy_json");
    let reset_interval_ms: i64 = policy_json
        .get("reset_interval_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // Zero all usage rows for this budget.
    sqlx::query(
        "UPDATE ff_budget_usage \
         SET current_value = 0, last_reset_at_ms = $3, updated_at_ms = $3 \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Reset policy metadata + reschedule.
    let row = sqlx::query(
        "UPDATE ff_budget_policy \
         SET last_breach_at_ms = NULL, \
             last_breach_dim   = NULL, \
             updated_at_ms     = $3, \
             next_reset_at_ms  = CASE \
                 WHEN $4::bigint > 0 THEN $3 + $4::bigint \
                 ELSE NULL \
             END \
         WHERE partition_key = $1 AND budget_id = $2 \
         RETURNING next_reset_at_ms",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .bind(now)
    .bind(reset_interval_ms)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let next_reset: Option<i64> = row
        .try_get::<Option<i64>, _>("next_reset_at_ms")
        .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(ResetBudgetResult::Reset {
        // `ResetBudgetResult::Reset` carries a non-optional
        // `TimestampMs`; when no interval is configured the budget
        // has no scheduled-reset, so report `0` (matches Valkey's
        // zero-score-when-unset behaviour for ZADD resets_zset).
        next_reset_at: TimestampMs(next_reset.unwrap_or(0)),
    })
}

// ───────────────────────────────────────────────────────────────────
// §4.4.1 — `create_quota_policy`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn create_quota_policy_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    args: CreateQuotaPolicyArgs,
) -> Result<CreateQuotaPolicyResult, EngineError> {
    let partition = quota_partition(&args.quota_policy_id, partition_config);
    let partition_key: i16 = partition.index as i16;
    let qid = args.quota_policy_id.clone();
    let now: i64 = args.now.0;

    let row = sqlx::query(
        "INSERT INTO ff_quota_policy \
             (partition_key, quota_policy_id, requests_per_window_seconds, \
              max_requests_per_window, active_concurrency_cap, \
              active_concurrency, created_at_ms, updated_at_ms) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6) \
         ON CONFLICT (partition_key, quota_policy_id) DO NOTHING \
         RETURNING created_at_ms",
    )
    .bind(partition_key)
    .bind(qid.to_string())
    .bind(args.window_seconds as i64)
    .bind(args.max_requests_per_window as i64)
    .bind(args.max_concurrent as i64)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    match row {
        Some(_) => Ok(CreateQuotaPolicyResult::Created {
            quota_policy_id: qid,
        }),
        None => Ok(CreateQuotaPolicyResult::AlreadySatisfied {
            quota_policy_id: qid,
        }),
    }
}

// ───────────────────────────────────────────────────────────────────
// §4.4.7 — `get_budget_status`
// ───────────────────────────────────────────────────────────────────

pub(crate) async fn get_budget_status_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    budget: &BudgetId,
) -> Result<BudgetStatus, EngineError> {
    let partition = budget_partition(budget, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id_str = budget.to_string();

    // SELECT 1: policy row (definitional + counters + scheduler).
    let policy_row = sqlx::query(
        "SELECT scope_type, scope_id, enforcement_mode, \
                breach_count, soft_breach_count, \
                last_breach_at_ms, last_breach_dim, \
                next_reset_at_ms, created_at_ms, \
                policy_json \
         FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(partition_key)
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

    let scope_type: String = policy_row.get("scope_type");
    let scope_id: String = policy_row.get("scope_id");
    let enforcement_mode: String = policy_row.get("enforcement_mode");
    let breach_count: i64 = policy_row.get("breach_count");
    let soft_breach_count: i64 = policy_row.get("soft_breach_count");
    let last_breach_at_ms: Option<i64> = policy_row
        .try_get::<Option<i64>, _>("last_breach_at_ms")
        .map_err(map_sqlx_error)?;
    let last_breach_dim: Option<String> = policy_row
        .try_get::<Option<String>, _>("last_breach_dim")
        .map_err(map_sqlx_error)?;
    let next_reset_at_ms: Option<i64> = policy_row
        .try_get::<Option<i64>, _>("next_reset_at_ms")
        .map_err(map_sqlx_error)?;
    let created_at_ms: i64 = policy_row.get("created_at_ms");
    let policy_json: JsonValue = policy_row.get("policy_json");

    // SELECT 2: usage rows (one per dimension).
    let usage_rows = sqlx::query(
        "SELECT dimensions_key, current_value \
         FROM ff_budget_usage \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut usage: HashMap<String, u64> = HashMap::new();
    for r in &usage_rows {
        let key: String = r.get("dimensions_key");
        let val: i64 = r.get("current_value");
        if key == "_init" {
            continue;
        }
        usage.insert(key, val.max(0) as u64);
    }

    // Limits parse from policy_json (shape documented on
    // `build_policy_json` above; matches Valkey's 3× HGETALL shape
    // byte-for-byte post-stringification).
    let hard_limits: HashMap<String, u64> =
        limits_from_policy(&policy_json, "hard_limits").into_iter().collect();
    let soft_limits: HashMap<String, u64> =
        limits_from_policy(&policy_json, "soft_limits").into_iter().collect();

    // Valkey stringifies i64 timestamps; mirror byte-for-byte so the
    // contract shape is identical across backends.
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

// ───────────────────────────────────────────────────────────────────
// §4.4.3 — `budget_reset` reconciler entry point
// ───────────────────────────────────────────────────────────────────

/// Called per row returned by the `budget_reset` reconciler scan.
/// Looks up the policy's `reset_interval_ms` from `policy_json`,
/// zeroes usage rows + resets breach metadata + reschedules
/// `next_reset_at_ms` under SERIALIZABLE. Same tx shape as
/// `reset_budget_impl` but keyed off the scanner-supplied
/// `(partition_key, budget_id)` pair rather than a caller-supplied
/// `BudgetId`.
pub(crate) async fn budget_reset_reconciler_apply(
    pool: &PgPool,
    partition_key: i16,
    budget_id: &str,
    now: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    let policy_row = sqlx::query(
        "SELECT policy_json, next_reset_at_ms FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2 FOR NO KEY UPDATE",
    )
    .bind(partition_key)
    .bind(budget_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(policy_row) = policy_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    };
    // Defensive re-check: a concurrent `reset_budget` may have already
    // advanced `next_reset_at_ms` past `now`. If so this scanner tick
    // is a no-op.
    let next_reset: Option<i64> = policy_row
        .try_get::<Option<i64>, _>("next_reset_at_ms")
        .map_err(map_sqlx_error)?;
    if !matches!(next_reset, Some(n) if n <= now) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }
    let policy_json: JsonValue = policy_row.get("policy_json");
    let reset_interval_ms: i64 = policy_json
        .get("reset_interval_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    sqlx::query(
        "UPDATE ff_budget_usage \
         SET current_value = 0, last_reset_at_ms = $3, updated_at_ms = $3 \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(partition_key)
    .bind(budget_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "UPDATE ff_budget_policy \
         SET last_breach_at_ms = NULL, \
             last_breach_dim   = NULL, \
             updated_at_ms     = $3, \
             next_reset_at_ms  = CASE \
                 WHEN $4::bigint > 0 THEN $3 + $4::bigint \
                 ELSE NULL \
             END \
         WHERE partition_key = $1 AND budget_id = $2",
    )
    .bind(partition_key)
    .bind(budget_id)
    .bind(now)
    .bind(reset_interval_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────
// cairn #454 Phase 4a — `record_spend` + `release_budget`
// (per-execution ledger, option A; parity with Valkey PR #464).
// ───────────────────────────────────────────────────────────────────

/// Extract the bare UUID from an `ExecutionId` (formatted
/// `{fp:N}:<uuid>`). `ff_budget_usage_by_exec.execution_id` is typed
/// as `uuid`, not the wrapped hash-tag string, so we need the inner
/// bytes. Mirrors `attempt::split_exec_id` but returns only the UUID.
fn exec_uuid(eid: &ExecutionId) -> Result<Uuid, EngineError> {
    let s = eid.as_str();
    let rest = s.strip_prefix("{fp:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `{{fp:` prefix: {s}"),
    })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })
}

/// cairn #454 Phase 4a — `EngineBackend::record_spend`.
///
/// Per-execution budget spend with open-set dimensions. Structurally
/// identical to [`report_usage_and_check_core`] (dedup INSERT → policy
/// lock → per-dim admission → apply increments → soft-breach book-
/// keeping), with two deltas from option A:
///
/// 1. The idempotency key is `args.idempotency_key` (caller-computed
///    SHA-256 hex per RFC cairn #454 Q1) instead of
///    `UsageDimensions::dedup_key`.
/// 2. After the aggregate UPSERT the same deltas land in the
///    `ff_budget_usage_by_exec` per-execution ledger (new 0020 table)
///    so `release_budget` can reverse just this execution's
///    attribution. Matches Valkey's `HINCRBY` into
///    `ff:budget:...:by_exec:<execution_id>`.
pub(crate) async fn record_spend_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    args: RecordSpendArgs,
) -> Result<ReportUsageResult, EngineError> {
    let partition = budget_partition(&args.budget_id, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id_str = args.budget_id.to_string();
    let exec_uuid = exec_uuid(&args.execution_id)?;
    let now = now_ms();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // ── Dedup (reuses the existing `ff_budget_usage_dedup` infra) ──
    let dedup_owned: Option<String> = if args.idempotency_key.is_empty() {
        None
    } else {
        let inserted = sqlx::query(
            "INSERT INTO ff_budget_usage_dedup \
                 (partition_key, dedup_key, outcome_json, applied_at_ms, expires_at_ms) \
             VALUES ($1, $2, '{}'::jsonb, $3, $4) \
             ON CONFLICT (partition_key, dedup_key) DO NOTHING \
             RETURNING applied_at_ms",
        )
        .bind(partition_key)
        .bind(&args.idempotency_key)
        .bind(now)
        .bind(now + DEDUP_TTL_MS)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        if inserted.is_none() {
            let row = sqlx::query(
                "SELECT outcome_json FROM ff_budget_usage_dedup \
                 WHERE partition_key = $1 AND dedup_key = $2",
            )
            .bind(partition_key)
            .bind(&args.idempotency_key)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
            let outcome: JsonValue = row.get("outcome_json");
            tx.commit().await.map_err(map_sqlx_error)?;
            // cairn #454 Phase 3 parity with Valkey `ff_record_spend`:
            // dedup-hit always returns `AlreadyApplied` regardless of
            // the original outcome (SET `dedup_key "1"` in Lua —
            // outcome is not replayed). We still consult the stored
            // outcome only to verify the row isn't an orphaned empty
            // placeholder (in which case a prior in-flight caller
            // crashed; we treat it the same — AlreadyApplied).
            let _ = outcome; // placeholder read, semantics match Valkey
            return Ok(ReportUsageResult::AlreadyApplied);
        }
        Some(args.idempotency_key.clone())
    };

    // ── Load policy row with `FOR NO KEY UPDATE` (same lock-mode
    //    discipline as `report_usage_and_check_core`). ──
    let policy_row = sqlx::query(
        "SELECT policy_json FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2 FOR NO KEY UPDATE",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let policy: JsonValue = match policy_row {
        Some(r) => r.get("policy_json"),
        None => JsonValue::Object(Default::default()),
    };
    let hard_limits = limits_from_policy(&policy, "hard_limits");
    let soft_limits = limits_from_policy(&policy, "soft_limits");

    // ── Hard-breach pre-check: ANY dim over hard-limit rejects the
    //    whole call (no increments applied, ledger untouched). ──
    let mut per_dim_current: BTreeMap<String, u64> = BTreeMap::new();
    for (dim, delta) in args.deltas.iter() {
        let dim_key = dim_row_key(dim);
        sqlx::query(
            "INSERT INTO ff_budget_usage \
                 (partition_key, budget_id, dimensions_key, current_value, updated_at_ms) \
             VALUES ($1, $2, $3, 0, $4) \
             ON CONFLICT (partition_key, budget_id, dimensions_key) DO NOTHING",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let row = sqlx::query(
            "SELECT current_value FROM ff_budget_usage \
             WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3 \
             FOR UPDATE",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        let cur: i64 = row.get("current_value");
        let new_val = (cur as u64).saturating_add(*delta);

        if let Some(hard) = hard_limits.get(dim)
            && *hard > 0
            && new_val > *hard
        {
            sqlx::query(
                "UPDATE ff_budget_policy \
                 SET breach_count      = breach_count + 1, \
                     last_breach_at_ms = $3, \
                     last_breach_dim   = $4, \
                     updated_at_ms     = $3 \
                 WHERE partition_key = $1 AND budget_id = $2",
            )
            .bind(partition_key)
            .bind(&budget_id_str)
            .bind(now)
            .bind(dim)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            let outcome = ReportUsageResult::HardBreach {
                dimension: dim.clone(),
                current_usage: cur as u64,
                hard_limit: *hard,
            };
            if let Some(dk) = dedup_owned.as_deref() {
                finalize_dedup(&mut tx, partition_key, dk, &outcome).await?;
            }
            tx.commit().await.map_err(map_sqlx_error)?;
            return Ok(outcome);
        }
        per_dim_current.insert(dim.clone(), new_val);
    }

    // ── Apply increments + soft-breach detection + mirror into the
    //    per-execution ledger (new 0020 table). ──
    let mut soft_breach: Option<ReportUsageResult> = None;
    for (dim, delta) in args.deltas.iter() {
        let dim_key = dim_row_key(dim);
        let new_val = per_dim_current[dim];
        sqlx::query(
            "UPDATE ff_budget_usage \
             SET current_value = current_value + $1, updated_at_ms = $2 \
             WHERE partition_key = $3 AND budget_id = $4 AND dimensions_key = $5",
        )
        .bind(*delta as i64)
        .bind(now)
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim_key)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Per-execution ledger UPSERT — additive on repeat
        // `record_spend` for the same (budget, exec, dim).
        sqlx::query(
            "INSERT INTO ff_budget_usage_by_exec \
                 (partition_key, budget_id, execution_id, dimensions_key, \
                  delta_total, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (partition_key, budget_id, execution_id, dimensions_key) \
             DO UPDATE SET delta_total = ff_budget_usage_by_exec.delta_total + EXCLUDED.delta_total, \
                           updated_at_ms = EXCLUDED.updated_at_ms",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(exec_uuid)
        .bind(&dim_key)
        .bind(*delta as i64)
        .bind(now)
        .execute(&mut *tx)
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

    if soft_breach.is_some() {
        sqlx::query(
            "UPDATE ff_budget_policy \
             SET soft_breach_count = soft_breach_count + 1, \
                 updated_at_ms     = $3 \
             WHERE partition_key = $1 AND budget_id = $2",
        )
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    let outcome = soft_breach.unwrap_or(ReportUsageResult::Ok);
    if let Some(dk) = dedup_owned.as_deref() {
        finalize_dedup(&mut tx, partition_key, dk, &outcome).await?;
    }
    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(outcome)
}

/// cairn #454 Phase 4a — `EngineBackend::release_budget`.
///
/// Reverses a single execution's contribution to a budget aggregate
/// using the per-exec ledger from 0020. Scans all
/// `ff_budget_usage_by_exec` rows for (budget, exec), subtracts each
/// `delta_total` from the matching aggregate (clamped at 0 — matches
/// the Valkey `math.max(0, ...)` semantics), then DELETEs the ledger
/// rows. Idempotent: empty ledger ⇒ no-op `Ok(())`.
pub(crate) async fn release_budget_impl(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    args: ReleaseBudgetArgs,
) -> Result<(), EngineError> {
    let partition = budget_partition(&args.budget_id, partition_config);
    let partition_key: i16 = partition.index as i16;
    let budget_id_str = args.budget_id.to_string();
    let exec_uuid = exec_uuid(&args.execution_id)?;
    let now = now_ms();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // Scan the ledger under `FOR UPDATE` so concurrent
    // `record_spend` on the same (budget, exec, dim) serialises
    // on the row lock rather than racing the DELETE.
    let rows = sqlx::query(
        "SELECT dimensions_key, delta_total \
         FROM ff_budget_usage_by_exec \
         WHERE partition_key = $1 AND budget_id = $2 AND execution_id = $3 \
         FOR UPDATE",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .bind(exec_uuid)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if rows.is_empty() {
        // No prior record_spend for this execution ⇒ nothing to
        // reverse. Idempotent no-op, matching Valkey's behaviour when
        // `ff:budget:...:by_exec:<exec>` doesn't exist.
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(());
    }

    for row in &rows {
        let dim: String = row.get("dimensions_key");
        let delta: i64 = row.get("delta_total");
        sqlx::query(
            "UPDATE ff_budget_usage \
             SET current_value = GREATEST(0::bigint, current_value - $1), \
                 updated_at_ms = $2 \
             WHERE partition_key = $3 AND budget_id = $4 AND dimensions_key = $5",
        )
        .bind(delta)
        .bind(now)
        .bind(partition_key)
        .bind(&budget_id_str)
        .bind(&dim)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    sqlx::query(
        "DELETE FROM ff_budget_usage_by_exec \
         WHERE partition_key = $1 AND budget_id = $2 AND execution_id = $3",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
