//! Typed FCALL wrappers for lease management functions.
//!
//! These wrap the Lua functions defined in `lua/lease.lua`.
//! Each function uses the `ff_function!` macro to generate an async fn
//! that builds KEYS/ARGV, calls FCALL, and parses the result.

use crate::error::ScriptError;
use ff_core::contracts::{
    MarkLeaseExpiredArgs, MarkLeaseExpiredResult, RenewLeaseArgs, RenewLeaseResult,
    RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::keys::ExecKeyContext;
use ff_core::types::TimestampMs;

use crate::result::{FcallResult, FromFcallResult};

// ─── FromFcallResult implementations ───

impl FromFcallResult for RenewLeaseResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua returns: ok(new_expires_at_string)
        let expires_str = r.field_str(0);
        let expires_ms: i64 = expires_str
            .parse()
            .map_err(|_| ScriptError::Parse(format!("invalid expires_at: {expires_str}")))?;
        Ok(RenewLeaseResult::Renewed {
            expires_at: TimestampMs::from_millis(expires_ms),
        })
    }
}

impl FromFcallResult for MarkLeaseExpiredResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua returns: ok("marked_expired") or ok_already_satisfied("reason")
        match r.status.as_str() {
            "OK" => Ok(MarkLeaseExpiredResult::MarkedExpired),
            "ALREADY_SATISFIED" => Ok(MarkLeaseExpiredResult::AlreadySatisfied {
                reason: r.field_str(0),
            }),
            other => Err(ScriptError::Parse(format!(
                "unexpected status from ff_mark_lease_expired_if_due: {other}"
            ))),
        }
    }
}

impl FromFcallResult for RevokeLeaseResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua returns: ok("revoked", lease_id, lease_epoch)
        //           or ok_already_satisfied("reason")
        match r.status.as_str() {
            "OK" => {
                // fields[0] = "revoked", fields[1] = lease_id, fields[2] = lease_epoch
                Ok(RevokeLeaseResult::Revoked {
                    lease_id: r.field_str(1),
                    lease_epoch: r.field_str(2),
                })
            }
            "ALREADY_SATISFIED" => Ok(RevokeLeaseResult::AlreadySatisfied {
                reason: r.field_str(0),
            }),
            other => Err(ScriptError::Parse(format!(
                "unexpected status from ff_revoke_lease: {other}"
            ))),
        }
    }
}

// ─── ff_function! invocations ───

ff_function! {
    /// Renew an active lease. Extends expires_at by lease_ttl_ms.
    ///
    /// KEYS(4): exec_core, lease_current, lease_history, lease_expiry_zset
    /// ARGV(7): execution_id, attempt_index, attempt_id, lease_id, lease_epoch,
    ///          lease_ttl_ms, lease_history_grace_ms
    pub ff_renew_lease(args: RenewLeaseArgs) -> RenewLeaseResult {
        keys(ctx: &ExecKeyContext) {
            ctx.core(),
            ctx.lease_current(),
            ctx.lease_history(),
            format!("ff:idx:{}:lease_expiry", ctx.hash_tag()),
        }
        // RFC #58.5: `fence` is Option<LeaseFence>. Renew hard-rejects
        // empty triples with `fence_required` — no operator override.
        argv {
            args.execution_id.to_string(),
            args.attempt_index.to_string(),
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(),
            args.lease_ttl_ms.to_string(),
            args.lease_history_grace_ms.to_string(),
        }
    }

    /// Mark a lease as expired if it is actually due.
    /// Called by the lease expiry scanner.
    ///
    /// KEYS(4): exec_core, lease_current, lease_expiry_zset, lease_history
    /// ARGV(1): execution_id
    pub ff_mark_lease_expired_if_due(args: MarkLeaseExpiredArgs) -> MarkLeaseExpiredResult {
        keys(ctx: &ExecKeyContext) {
            ctx.core(),
            ctx.lease_current(),
            format!("ff:idx:{}:lease_expiry", ctx.hash_tag()),
            ctx.lease_history(),
        }
        argv {
            args.execution_id.to_string(),
        }
    }

    /// Revoke an active lease (operator-initiated).
    ///
    /// KEYS(5): exec_core, lease_current, lease_history, lease_expiry_zset, worker_leases
    /// ARGV(3): execution_id, expected_lease_id, revoke_reason
    pub ff_revoke_lease(args: RevokeLeaseArgs) -> RevokeLeaseResult {
        keys(ctx: &ExecKeyContext) {
            ctx.core(),
            ctx.lease_current(),
            ctx.lease_history(),
            format!("ff:idx:{}:lease_expiry", ctx.hash_tag()),
            format!("ff:idx:{}:worker:{}:leases", ctx.hash_tag(), args.worker_instance_id),
        }
        argv {
            args.execution_id.to_string(),
            args.expected_lease_id.as_deref().unwrap_or("").to_string(),
            args.reason.clone(),
        }
    }
}
