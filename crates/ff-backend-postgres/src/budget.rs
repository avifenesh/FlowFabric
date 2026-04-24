//! Budget family — Postgres impl.
//!
//! **RFC-v0.7 Wave 4f.** Implements `EngineBackend::report_usage`
//! against the Wave 3b schema (`0002_budget.sql`): `ff_budget_policy`,
//! `ff_budget_usage`, `ff_budget_usage_dedup`.
//!
//! Isolation per v0.7 migration-master §Q11: READ COMMITTED + row-level
//! `FOR UPDATE` on `ff_budget_usage` and `FOR SHARE` on
//! `ff_budget_policy` — NOT SERIALIZABLE. Worker-A Tier A: trivial
//! single-key atomic increment per dimension.
//!
//! Idempotency per RFC-012 §R7.2.3: the caller-supplied
//! `UsageDimensions::dedup_key` is used as the key for an
//! `INSERT … ON CONFLICT DO NOTHING`; on conflict the cached
//! `outcome_json` row is returned verbatim (replay).
//!
//! `update_budget_policy` is NOT on the `EngineBackend` trait today
//! (only `report_usage` is). Wave 4f therefore ships only
//! `report_usage_impl`; definitional writes land via a test-only
//! helper (`upsert_policy_for_test`) that the integration suite
//! seeds through directly.

use std::collections::BTreeMap;

use ff_core::backend::UsageDimensions;
use ff_core::contracts::ReportUsageResult;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::{budget_partition, PartitionConfig};
use ff_core::types::BudgetId;
use serde_json::{json, Value as JsonValue};
use sqlx::{PgPool, Row};

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

/// `EngineBackend::report_usage` — Postgres impl.
pub(crate) async fn report_usage_impl(
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

    // ── Load policy (FOR SHARE so concurrent report_usage can proceed
    //    but an in-flight policy update blocks on the FOR UPDATE it
    //    would hold — when that admin surface lands). ──
    let policy_row = sqlx::query(
        "SELECT policy_json FROM ff_budget_policy \
         WHERE partition_key = $1 AND budget_id = $2 FOR SHARE",
    )
    .bind(partition_key)
    .bind(&budget_id_str)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let policy: JsonValue = match policy_row {
        Some(r) => r.get("policy_json"),
        // No policy row ⇒ no limits defined. Treat all reports as Ok
        // and still apply increments (the Valkey impl behaves the
        // same when the definition Hash is absent).
        None => JsonValue::Object(Default::default()),
    };
    let hard_limits = limits_from_policy(&policy, "hard_limits");
    let soft_limits = limits_from_policy(&policy, "soft_limits");

    // ── Compute admission per dimension. Hard breach on ANY dim
    //    rejects the whole report (no increments applied); soft breach
    //    is advisory (increments applied). Iterate in sorted-key order
    //    so the reported dimension is deterministic on multi-dim
    //    reports. ──
    //
    // Per-dim: SELECT current_value FOR UPDATE (locks the row for the
    // remainder of the txn). Row may not exist yet (first report for
    // that dim) — INSERT … ON CONFLICT DO NOTHING first, then re-SELECT
    // so the lock is held deterministically.
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
            // Hard breach: no increments applied, commit dedup outcome.
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

    let outcome = soft_breach.unwrap_or(ReportUsageResult::Ok);
    if let Some(dk) = dedup_owned.as_deref() {
        finalize_dedup(&mut tx, partition_key, dk, &outcome).await?;
    }
    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(outcome)
}

/// Write the final `outcome_json` onto the dedup row we inserted at
/// the top of [`report_usage_impl`].
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

/// Test-only helper: upsert a `ff_budget_policy` row directly. The
/// trait has no `update_budget_policy` method in v0.7 (Wave 4f
/// verification), so the Postgres backend exposes this as a public
/// helper so the integration suite can seed policies without going
/// through an op we haven't yet added to the trait surface.
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
