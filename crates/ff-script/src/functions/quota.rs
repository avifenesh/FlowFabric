//! Typed FCALL wrapper for quota admission function (lua/quota.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::QuotaKeyContext;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for quota admission check on {q:K}.
pub struct QuotaOpKeys<'a> {
    pub ctx: &'a QuotaKeyContext,
    pub dimension: &'a str,
    pub execution_id: &'a ff_core::types::ExecutionId,
}

// ─── ff_create_quota_policy ───────────────────────────────────────────
//
// Lua KEYS (5): quota_def, quota_window_zset, quota_concurrency_counter,
//               admitted_set, quota_policies_index
// Lua ARGV (5): quota_policy_id, window_seconds, max_requests_per_window,
//               max_concurrent, now_ms

ff_function! {
    pub ff_create_quota_policy(args: CreateQuotaPolicyArgs) -> CreateQuotaPolicyResult {
        keys(k: &QuotaOpKeys<'_>) {
            k.ctx.definition(),
            k.ctx.window(k.dimension),
            k.ctx.concurrency(),
            k.ctx.admitted_set(),
            ff_core::keys::quota_policies_index(k.ctx.hash_tag()),
        }
        argv {
            args.quota_policy_id.to_string(),
            args.window_seconds.to_string(),
            args.max_requests_per_window.to_string(),
            args.max_concurrent.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for CreateQuotaPolicyResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let id_str = r.field_str(0);
        let qid = ff_core::types::QuotaPolicyId::parse(&id_str)
            .map_err(|e| ScriptError::Parse(format!("invalid quota_policy_id: {e}")))?;
        match r.status.as_str() {
            "OK" => Ok(CreateQuotaPolicyResult::Created { quota_policy_id: qid }),
            "ALREADY_SATISFIED" => Ok(CreateQuotaPolicyResult::AlreadySatisfied { quota_policy_id: qid }),
            _ => Err(ScriptError::Parse(format!("unexpected status: {}", r.status))),
        }
    }
}

// ─── ff_check_admission_and_record ────────────────────────────────────
//
// Lua KEYS (5): window_zset, concurrency_counter, quota_def, admitted_guard,
//               admitted_set
// Lua ARGV (6): now_ms, window_seconds, rate_limit, concurrency_cap,
//               execution_id, jitter_ms

ff_function! {
    pub ff_check_admission_and_record(args: CheckAdmissionArgs) -> CheckAdmissionResult {
        keys(k: &QuotaOpKeys<'_>) {
            k.ctx.window(k.dimension),
            k.ctx.concurrency(),
            k.ctx.definition(),
            k.ctx.admitted(k.execution_id),
            k.ctx.admitted_set(),
        }
        argv {
            args.now.to_string(),
            args.window_seconds.to_string(),
            args.rate_limit.to_string(),
            args.concurrency_cap.to_string(),
            args.execution_id.to_string(),
            args.jitter_ms.unwrap_or(0).to_string(),
        }
    }
}

impl FromFcallResult for CheckAdmissionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        // Domain-specific return: {"ADMITTED"}, {"ALREADY_ADMITTED"},
        // {"RATE_EXCEEDED", retry_after_ms}, {"CONCURRENCY_EXCEEDED"}
        let arr = match raw {
            ferriskey::Value::Array(arr) => arr,
            _ => return Err(ScriptError::Parse("expected Array".into())),
        };
        let status = match arr.first() {
            Some(Ok(ferriskey::Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
            Some(Ok(ferriskey::Value::SimpleString(s))) => s.clone(),
            _ => return Err(ScriptError::Parse("expected status string".into())),
        };
        match status.as_str() {
            "ADMITTED" => Ok(CheckAdmissionResult::Admitted),
            "ALREADY_ADMITTED" => Ok(CheckAdmissionResult::AlreadyAdmitted),
            "RATE_EXCEEDED" => {
                let retry_str = match arr.get(1) {
                    Some(Ok(ferriskey::Value::BulkString(b))) => {
                        String::from_utf8_lossy(b).into_owned()
                    }
                    Some(Ok(ferriskey::Value::Int(n))) => n.to_string(),
                    _ => "0".to_string(),
                };
                let retry_after: u64 = retry_str.parse().unwrap_or(0);
                Ok(CheckAdmissionResult::RateExceeded {
                    retry_after_ms: retry_after,
                })
            }
            "CONCURRENCY_EXCEEDED" => Ok(CheckAdmissionResult::ConcurrencyExceeded),
            _ => Err(ScriptError::Parse(format!(
                "unknown admission status: {status}"
            ))),
        }
    }
}
