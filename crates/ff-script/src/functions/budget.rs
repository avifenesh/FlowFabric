//! Typed FCALL wrappers for budget functions (lua/budget.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};

use crate::result::{FcallResult, FromFcallResult};

/// Key context for budget operations on {b:M}.
pub struct BudgetOpKeys<'a> {
    pub usage_key: &'a str,
    pub limits_key: &'a str,
    pub def_key: &'a str,
}

/// Key context for budget block/unblock on {p:N}.
pub struct BlockOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a ff_core::types::LaneId,
}

// ─── ff_report_usage_and_check ────────────────────────────────────────
//
// Lua KEYS (3): budget_usage, budget_limits, budget_def
// Lua ARGV (variable): dimension_count, dim_1..dim_N, delta_1..delta_N, now_ms
//
// Manual implementation because ff_function! macro cannot handle variable-length
// ARGV. The Lua reads positional args: [dim_count, dim1..dimN, delta1..deltaN, now_ms].

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

    // Build flat ARGV: [dim_count, dim1..dimN, delta1..deltaN, now_ms]
    let dim_count = args.dimensions.len();
    let mut argv: Vec<String> = Vec::with_capacity(2 + dim_count * 2);
    argv.push(dim_count.to_string());
    for dim in &args.dimensions {
        argv.push(dim.clone());
    }
    for delta in &args.deltas {
        argv.push(delta.to_string());
    }
    argv.push(args.now.to_string());

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = conn
        .fcall::<ferriskey::Value>("ff_report_usage_and_check", &key_refs, &argv_refs)
        .await
        .map_err(|e| ScriptError::Valkey(e.to_string()))?;
    <ReportUsageResult as FromFcallResult>::from_fcall_result(&raw)
}

impl FromFcallResult for ReportUsageResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        // Domain-specific return: {"OK"}, {"SOFT_BREACH", dim, action},
        // {"HARD_BREACH", dim, action, current, limit}
        let arr = match raw {
            ferriskey::Value::Array(arr) => arr,
            _ => return Err(ScriptError::Parse("expected Array".into())),
        };
        let status = match arr.first() {
            Some(Ok(ferriskey::Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
            _ => return Err(ScriptError::Parse("expected status string".into())),
        };
        match status.as_str() {
            "OK" => Ok(ReportUsageResult::Ok),
            "SOFT_BREACH" => {
                let dim = field_str_from_arr(arr, 1);
                let action = field_str_from_arr(arr, 2);
                Ok(ReportUsageResult::SoftBreach { dimension: dim, action })
            }
            "HARD_BREACH" => {
                let dim = field_str_from_arr(arr, 1);
                let action = field_str_from_arr(arr, 2);
                let current: u64 = field_str_from_arr(arr, 3).parse().unwrap_or(0);
                let limit: u64 = field_str_from_arr(arr, 4).parse().unwrap_or(0);
                Ok(ReportUsageResult::HardBreach {
                    dimension: dim,
                    action,
                    current_usage: current,
                    hard_limit: limit,
                })
            }
            _ => Err(ScriptError::Parse(format!("unknown budget status: {status}"))),
        }
    }
}

// ─── ff_reset_budget ──────────────────────────────────────────────────
//
// Lua KEYS (3): budget_def, budget_usage, budget_resets_zset
// Lua ARGV (2): budget_id, now_ms

ff_function! {
    pub ff_reset_budget(args: ResetBudgetArgs) -> ResetBudgetResult {
        keys(k: &BudgetOpKeys<'_>) {
            k.def_key.to_string(),
            k.usage_key.to_string(),
            k.limits_key.to_string(),  // reused as resets_zset
        }
        argv {
            args.budget_id.to_string(),
            args.now.to_string(),
        }
    }
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

// ─── Helper ───────────────────────────────────────────────────────────

fn field_str_from_arr(arr: &[Result<ferriskey::Value, ferriskey::Error>], index: usize) -> String {
    match arr.get(index) {
        Some(Ok(ferriskey::Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(ferriskey::Value::SimpleString(s))) => s.clone(),
        Some(Ok(ferriskey::Value::Int(n))) => n.to_string(),
        _ => String::new(),
    }
}
