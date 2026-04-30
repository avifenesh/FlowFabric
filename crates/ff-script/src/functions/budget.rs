//! Typed FCALL wrappers for budget functions (lua/budget.lua).

use ff_core::contracts::*;
use crate::error::ScriptError;
use ff_core::keys::{usage_dedup_key, ExecKeyContext, IndexKeys};

use crate::result::{FcallResult, FromFcallResult};

/// Single source of truth for the budget dimension cap (#104).
///
/// Enforced at both the HTTP boundary in `ff-server` (which re-exports this
/// constant) and inside the typed FCALL wrappers below, so direct
/// script-helper callers (tests, tools, alternate services) cannot reach
/// Valkey with an unbounded `dim_count` by skipping the REST layer.
///
/// 64 is generously above any legitimate scoping dimension count
/// (org/tenant/project/region/lane/tier/…) while bounding worst-case
/// FCALL ARGV to ~200 strings — well below Valkey argv limits.
pub const MAX_BUDGET_DIMENSIONS: usize = 64;

/// Key context for budget operations on {b:M}.
///
/// `hash_tag` is the budget partition's Valkey hash-tag (e.g. `{b:3}`,
/// braces included) so the typed wrapper can wrap any per-call `dedup_key`
/// into a slot-co-located `ff:usagededup:{b:M}:<dedup_id>` key without the
/// caller needing to know the wrapping format. The canonical source for
/// the value to pass is [`ff_core::keys::BudgetKeyContext::hash_tag`]
/// (#108).
pub struct BudgetOpKeys<'a> {
    pub usage_key: &'a str,
    pub limits_key: &'a str,
    pub def_key: &'a str,
    pub hash_tag: &'a str,
}

/// Key context for budget block/unblock on {p:N}.
pub struct BlockOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a ff_core::types::LaneId,
}

// ─── ff_create_budget ─────────────────────────────────────────────────
//
// Lua KEYS (5): budget_def, budget_limits, budget_usage, budget_resets_zset,
//               budget_policies_index
// Lua ARGV (variable): budget_id, scope_type, scope_id, enforcement_mode,
//   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
//   dimension_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
//
// Manual implementation because ff_function! macro cannot handle variable-length ARGV.

pub async fn ff_create_budget(
    conn: &ferriskey::Client,
    k: &BudgetOpKeys<'_>,
    resets_zset: &str,
    policies_index: &str,
    args: &CreateBudgetArgs,
) -> Result<CreateBudgetResult, ScriptError> {
    let keys: Vec<String> = vec![
        k.def_key.to_string(),
        k.limits_key.to_string(),
        k.usage_key.to_string(),
        resets_zset.to_string(),
        policies_index.to_string(),
    ];

    let dim_count = args.dimensions.len();
    // Cap ARGV before allocation — see MAX_BUDGET_DIMENSIONS (#104).
    if dim_count > MAX_BUDGET_DIMENSIONS {
        return Err(ScriptError::Parse {
            fcall: "ff_create_budget".into(),
            execution_id: None,
            message: format!(
            "too_many_dimensions: limit={}, got={}",
            MAX_BUDGET_DIMENSIONS, dim_count
        ),
        });
    }
    if args.hard_limits.len() != dim_count {
        return Err(ScriptError::Parse {
            fcall: "ff_create_budget".into(),
            execution_id: None,
            message: format!(
            "dimension_limit_array_mismatch: dimensions={} hard_limits={}",
            dim_count,
            args.hard_limits.len()
        ),
        });
    }
    if args.soft_limits.len() != dim_count {
        return Err(ScriptError::Parse {
            fcall: "ff_create_budget".into(),
            execution_id: None,
            message: format!(
            "dimension_limit_array_mismatch: dimensions={} soft_limits={}",
            dim_count,
            args.soft_limits.len()
        ),
        });
    }
    // ARGV: budget_id, scope_type, scope_id, enforcement_mode,
    //   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
    //   dim_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
    let mut argv: Vec<String> = Vec::with_capacity(9 + dim_count * 3);
    argv.push(args.budget_id.to_string());
    argv.push(args.scope_type.clone());
    argv.push(args.scope_id.clone());
    argv.push(args.enforcement_mode.clone());
    argv.push(args.on_hard_limit.clone());
    argv.push(args.on_soft_limit.clone());
    argv.push(args.reset_interval_ms.to_string());
    argv.push(args.now.to_string());
    argv.push(dim_count.to_string());
    for dim in &args.dimensions {
        argv.push(dim.clone());
    }
    for hard in &args.hard_limits {
        argv.push(hard.to_string());
    }
    for soft in &args.soft_limits {
        argv.push(soft.to_string());
    }

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = conn
        .fcall::<ferriskey::Value>("ff_create_budget", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    <CreateBudgetResult as FromFcallResult>::from_fcall_result(&raw)
}

impl FromFcallResult for CreateBudgetResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let id_str = r.field_str(0);
        let budget_id = ff_core::types::BudgetId::parse(&id_str)
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_create_budget".into(),
                execution_id: None,
                message: format!("invalid budget_id: {e}"),
            })?;
        match r.status.as_str() {
            "OK" => Ok(CreateBudgetResult::Created { budget_id }),
            "ALREADY_SATISFIED" => Ok(CreateBudgetResult::AlreadySatisfied { budget_id }),
            _ => Err(ScriptError::Parse {
                fcall: "ff_create_budget".into(),
                execution_id: None,
                message: format!("unexpected status: {}", r.status),
            }),
        }
    }
}

// ─── ff_report_usage_and_check ────────────────────────────────────────
//
// Lua KEYS (3): budget_usage, budget_limits, budget_def
// Lua ARGV (variable): dimension_count, dim_1..dim_N, delta_1..delta_N, now_ms, [dedup_key]
//
// Manual implementation because ff_function! macro cannot handle variable-length
// ARGV. The Lua reads positional args: [dim_count, dim1..dimN, delta1..deltaN, now_ms, dedup_key].

pub async fn ff_report_usage_and_check(
    conn: &ferriskey::Client,
    k: &BudgetOpKeys<'_>,
    args: &ReportUsageArgs,
) -> Result<ReportUsageResult, ScriptError> {
    let keys: Vec<String> = vec![
        k.usage_key.to_string(),
        k.limits_key.to_string(),
        k.def_key.to_string(),
    ];

    // Build flat ARGV: [dim_count, dim1..dimN, delta1..deltaN, now_ms, dedup_key]
    let dim_count = args.dimensions.len();
    // Cap ARGV before allocation — see MAX_BUDGET_DIMENSIONS (#104).
    if dim_count > MAX_BUDGET_DIMENSIONS {
        return Err(ScriptError::Parse {
            fcall: "ff_report_usage_and_check".into(),
            execution_id: None,
            message: format!(
            "too_many_dimensions: limit={}, got={}",
            MAX_BUDGET_DIMENSIONS, dim_count
        ),
        });
    }
    if args.deltas.len() != dim_count {
        return Err(ScriptError::Parse {
            fcall: "ff_report_usage_and_check".into(),
            execution_id: None,
            message: format!(
            "dimension_delta_array_mismatch: dimensions={} deltas={}",
            dim_count,
            args.deltas.len()
        ),
        });
    }
    let mut argv: Vec<String> = Vec::with_capacity(3 + dim_count * 2);
    argv.push(dim_count.to_string());
    for dim in &args.dimensions {
        argv.push(dim.clone());
    }
    for delta in &args.deltas {
        argv.push(delta.to_string());
    }
    argv.push(args.now.to_string());
    // #108: wrap dedup_key with the budget hash-tag so it co-locates with
    // `k.usage_key`/`k.limits_key`/`k.def_key` on the same cluster slot and
    // matches the format produced by `ff-server` + `ff-sdk` at the REST
    // boundary. Empty/missing dedup_key forwards as empty string
    // (Lua disables dedup in that branch).
    let dedup_key_val = args.dedup_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| usage_dedup_key(k.hash_tag, s))
        .unwrap_or_default();
    argv.push(dedup_key_val);

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = conn
        .fcall::<ferriskey::Value>("ff_report_usage_and_check", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    <ReportUsageResult as FromFcallResult>::from_fcall_result(&raw)
}

impl FromFcallResult for ReportUsageResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        match r.status.as_str() {
            "OK" => Ok(ReportUsageResult::Ok),
            "ALREADY_APPLIED" => Ok(ReportUsageResult::AlreadyApplied),
            "SOFT_BREACH" => {
                let dim = r.field_str(0);
                let current: u64 = r.field_str(1).parse().unwrap_or(0);
                let limit: u64 = r.field_str(2).parse().unwrap_or(0);
                Ok(ReportUsageResult::SoftBreach { dimension: dim, current_usage: current, soft_limit: limit })
            }
            "HARD_BREACH" => {
                let dim = r.field_str(0);
                let current: u64 = r.field_str(1).parse().unwrap_or(0);
                let limit: u64 = r.field_str(2).parse().unwrap_or(0);
                Ok(ReportUsageResult::HardBreach {
                    dimension: dim,
                    current_usage: current,
                    hard_limit: limit,
                })
            }
            _ => Err(ScriptError::Parse {
                fcall: "ff_report_usage_and_check".into(),
                execution_id: None,
                message: format!("unknown budget status: {}", r.status),
            }),
        }
    }
}

// ─── ff_record_spend ──────────────────────────────────────────────────
//
// Open-set (cairn #454 Phase 3a) variant of `ff_report_usage_and_check`.
// Accepts a `BTreeMap<String, u64>` of tenant-defined dimension deltas
// and reuses `ReportUsageResult` — the four-variant outcome space is
// identical.
//
// Lua KEYS (3): budget_usage, budget_limits, budget_def
// Lua ARGV: pair_count, dim_1, delta_1, ..., dim_N, delta_N, now_ms, dedup_key

pub async fn ff_record_spend(
    conn: &ferriskey::Client,
    k: &BudgetOpKeys<'_>,
    args: &RecordSpendArgs,
    now: ff_core::types::TimestampMs,
) -> Result<ReportUsageResult, ScriptError> {
    let keys: Vec<String> = vec![
        k.usage_key.to_string(),
        k.limits_key.to_string(),
        k.def_key.to_string(),
    ];

    let pair_count = args.deltas.len();
    if pair_count > MAX_BUDGET_DIMENSIONS {
        return Err(ScriptError::Parse {
            fcall: "ff_record_spend".into(),
            execution_id: Some(args.execution_id.to_string()),
            message: format!(
                "too_many_dimensions: limit={}, got={}",
                MAX_BUDGET_DIMENSIONS, pair_count
            ),
        });
    }

    let mut argv: Vec<String> = Vec::with_capacity(3 + pair_count * 2);
    argv.push(pair_count.to_string());
    for (dim, delta) in &args.deltas {
        argv.push(dim.clone());
        argv.push(delta.to_string());
    }
    argv.push(now.to_string());
    // Wrap the caller-supplied idempotency key with the budget hash-tag
    // so it co-locates on the same cluster slot as the usage/limits/def
    // keys — matches `ff_report_usage_and_check` dedup behaviour (#108).
    let dedup_key_val = if args.idempotency_key.is_empty() {
        String::new()
    } else {
        usage_dedup_key(k.hash_tag, &args.idempotency_key)
    };
    argv.push(dedup_key_val);

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = conn
        .fcall::<ferriskey::Value>("ff_record_spend", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    <ReportUsageResult as FromFcallResult>::from_fcall_result(&raw)
}

// ─── ff_reset_budget ──────────────────────────────────────────────────
//
// Lua KEYS (3): budget_def, budget_usage, budget_resets_zset
// Lua ARGV (2): budget_id, now_ms
//
// Manual implementation: BudgetOpKeys doesn't carry resets_zset, so we
// accept it as a separate parameter (same pattern as ff_create_budget).

pub async fn ff_reset_budget(
    conn: &ferriskey::Client,
    k: &BudgetOpKeys<'_>,
    resets_zset: &str,
    args: &ResetBudgetArgs,
) -> Result<ResetBudgetResult, ScriptError> {
    let keys: Vec<String> = vec![
        k.def_key.to_string(),
        k.usage_key.to_string(),
        resets_zset.to_string(),
    ];
    let argv: Vec<String> = vec![
        args.budget_id.to_string(),
        args.now.to_string(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = conn
        .fcall::<ferriskey::Value>("ff_reset_budget", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    <ResetBudgetResult as FromFcallResult>::from_fcall_result(&raw)
}

impl FromFcallResult for ResetBudgetResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let next_str = r.field_str(0);
        let next_ms: i64 = next_str.parse().unwrap_or(0);
        Ok(ResetBudgetResult::Reset {
            next_reset_at: ff_core::types::TimestampMs::from_millis(next_ms),
        })
    }
}

// ─── ff_block_execution_for_admission ─────────────────────────────────
//
// Lua KEYS (3): exec_core, eligible_zset, target_blocked_zset
// Lua ARGV (4): execution_id, blocking_reason, blocking_detail, now_ms

ff_function! {
    pub ff_block_execution_for_admission(args: BlockExecutionArgs) -> BlockExecutionResult {
        keys(k: &BlockOpKeys<'_>) {
            k.ctx.core(),
            k.idx.lane_eligible(k.lane_id),
            {
                match args.blocking_reason.as_str() {
                    "waiting_for_budget" => k.idx.lane_blocked_budget(k.lane_id),
                    "waiting_for_quota" => k.idx.lane_blocked_quota(k.lane_id),
                    _ => k.idx.lane_blocked_budget(k.lane_id),
                }
            },
        }
        argv {
            args.execution_id.to_string(),
            args.blocking_reason.clone(),
            args.blocking_detail.clone().unwrap_or_default(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for BlockExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        Ok(BlockExecutionResult::Blocked)
    }
}

// ─── ff_unblock_execution ─────────────────────────────────────────────
//
// Lua KEYS (3): exec_core, source_blocked_zset, eligible_zset
// Lua ARGV (3): execution_id, now_ms, expected_blocking_reason

ff_function! {
    pub ff_unblock_execution(args: UnblockExecutionArgs) -> UnblockExecutionResult {
        keys(k: &BlockOpKeys<'_>) {
            k.ctx.core(),
            {
                match args.expected_blocking_reason.as_deref().unwrap_or("waiting_for_budget") {
                    "waiting_for_budget" => k.idx.lane_blocked_budget(k.lane_id),
                    "waiting_for_quota" => k.idx.lane_blocked_quota(k.lane_id),
                    _ => k.idx.lane_blocked_budget(k.lane_id),
                }
            },
            k.idx.lane_eligible(k.lane_id),
        }
        argv {
            args.execution_id.to_string(),
            args.now.to_string(),
            args.expected_blocking_reason.clone().unwrap_or_default(),
        }
    }
}

impl FromFcallResult for UnblockExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        Ok(UnblockExecutionResult::Unblocked)
    }
}

