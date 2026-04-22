//! Typed FCALL wrappers for suspension and waitpoint functions
//! (lua/suspension.lua).

use crate::error::ScriptError;
use ff_core::contracts::*;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::state::PublicState;
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for suspension operations.
/// Needs exec keys + index keys + lane + worker_instance_id (for lease release).
pub struct SuspendOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
    pub worker_instance_id: &'a WorkerInstanceId,
}

// ─── ff_suspend_execution ─────────────────────────────────────────────
//
// Lua KEYS (17): exec_core, attempt_record, lease_current, lease_history,
//                lease_expiry_zset, worker_leases, suspension_current,
//                waitpoint_hash, waitpoint_signals, suspension_timeout_zset,
//                pending_wp_expiry_zset, active_index, suspended_zset,
//                waitpoint_history, wp_condition, attempt_timeout_zset,
//                hmac_secrets
// Lua ARGV (17): execution_id, attempt_index, attempt_id, lease_id,
//                lease_epoch, suspension_id, waitpoint_id, waitpoint_key,
//                reason_code, requested_by, timeout_at, resume_condition_json,
//                resume_policy_json, continuation_metadata_pointer,
//                use_pending_waitpoint, timeout_behavior, lease_history_maxlen

ff_function! {
    pub ff_suspend_execution(args: SuspendExecutionArgs) -> SuspendExecutionResult {
        keys(k: &SuspendOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.attempt_hash(args.attempt_index),                    // 2
            k.ctx.lease_current(),                                     // 3
            k.ctx.lease_history(),                                     // 4
            k.idx.lease_expiry(),                                      // 5
            k.idx.worker_leases(k.worker_instance_id),                 // 6
            k.ctx.suspension_current(),                                // 7
            k.ctx.waitpoint(&args.waitpoint_id),                       // 8
            k.ctx.waitpoint_signals(&args.waitpoint_id),               // 9
            k.idx.suspension_timeout(),                                // 10
            k.idx.pending_waitpoint_expiry(),                          // 11
            k.idx.lane_active(k.lane_id),                              // 12
            k.idx.lane_suspended(k.lane_id),                           // 13
            k.ctx.waitpoints(),                                        // 14
            k.ctx.waitpoint_condition(&args.waitpoint_id),             // 15
            k.idx.attempt_timeout(),                                   // 16
            k.idx.waitpoint_hmac_secrets(),                            // 17
        }
        // RFC #58.5: `fence` is Option<LeaseFence>. Suspend hard-rejects
        // empty triples with `fence_required` — no operator override.
        argv {
            args.execution_id.to_string(),                             // 1
            args.attempt_index.to_string(),                            // 2
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),  // 3
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),    // 4
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(), // 5
            args.suspension_id.to_string(),                            // 6
            args.waitpoint_id.to_string(),                             // 7
            args.waitpoint_key.clone(),                                // 8
            args.reason_code.clone(),                                  // 9
            args.requested_by.clone(),                                 // 10
            args.timeout_at.map(|t| t.to_string()).unwrap_or_default(), // 11
            args.resume_condition_json.clone(),                        // 12
            args.resume_policy_json.clone(),                           // 13
            args.continuation_metadata_pointer.clone().unwrap_or_default(), // 14
            if args.use_pending_waitpoint { "1".into() } else { String::new() }, // 15
            args.timeout_behavior.clone(),                             // 16
            "1000".to_string(),                                        // 17 lease_history_maxlen
        }
    }
}

impl FromFcallResult for SuspendExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?;
        // ALREADY_SATISFIED: {1, "ALREADY_SATISFIED", suspension_id, waitpoint_id, waitpoint_key, waitpoint_token}
        if r.status == "ALREADY_SATISFIED" {
            let sid = SuspensionId::parse(&r.field_str(0))
                .map_err(|e| ScriptError::Parse {
                    fcall: "ff_suspend_execution".into(),
                    execution_id: None,
                    message: format!("bad suspension_id: {e}"),
                })?;
            let wid = WaitpointId::parse(&r.field_str(1))
                .map_err(|e| ScriptError::Parse {
                    fcall: "ff_suspend_execution".into(),
                    execution_id: None,
                    message: format!("bad waitpoint_id: {e}"),
                })?;
            let wkey = r.field_str(2);
            let token = WaitpointToken::new(r.field_str(3));
            return Ok(SuspendExecutionResult::AlreadySatisfied {
                suspension_id: sid,
                waitpoint_id: wid,
                waitpoint_key: wkey,
                waitpoint_token: token,
            });
        }
        let r = r.into_success()?;
        // ok(suspension_id, waitpoint_id, waitpoint_key, waitpoint_token)
        let sid = SuspensionId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_suspend_execution".into(),
                execution_id: None,
                message: format!("bad suspension_id: {e}"),
            })?;
        let wid = WaitpointId::parse(&r.field_str(1))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_suspend_execution".into(),
                execution_id: None,
                message: format!("bad waitpoint_id: {e}"),
            })?;
        let wkey = r.field_str(2);
        let token = WaitpointToken::new(r.field_str(3));
        Ok(SuspendExecutionResult::Suspended {
            suspension_id: sid,
            waitpoint_id: wid,
            waitpoint_key: wkey,
            waitpoint_token: token,
        })
    }
}

// ─── ff_resume_execution ──────────────────────────────────────────────
//
// Lua KEYS (8): exec_core, suspension_current, waitpoint_hash,
//               waitpoint_signals, suspension_timeout_zset,
//               eligible_zset, delayed_zset, suspended_zset
// Lua ARGV (3): execution_id, trigger_type, resume_delay_ms

/// Key context for resume — caller must pre-read current_waitpoint_id from
/// exec_core so the correct waitpoint keys can be passed to the Lua.
pub struct ResumeOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
    pub waitpoint_id: &'a WaitpointId,
}

ff_function! {
    pub ff_resume_execution(args: ResumeExecutionArgs) -> ResumeExecutionResult {
        keys(k: &ResumeOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.suspension_current(),                                // 2
            k.ctx.waitpoint(k.waitpoint_id),                           // 3
            k.ctx.waitpoint_signals(k.waitpoint_id),                   // 4
            k.idx.suspension_timeout(),                                // 5
            k.idx.lane_eligible(k.lane_id),                            // 6
            k.idx.lane_delayed(k.lane_id),                             // 7
            k.idx.lane_suspended(k.lane_id),                           // 8
        }
        argv {
            args.execution_id.to_string(),                             // 1
            args.trigger_type.clone(),                                 // 2
            args.resume_delay_ms.to_string(),                          // 3
        }
    }
}

impl FromFcallResult for ResumeExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(public_state)
        let ps_str = r.field_str(0);
        let public_state = parse_public_state(&ps_str)?;
        Ok(ResumeExecutionResult::Resumed { public_state })
    }
}

// ─── ff_create_pending_waitpoint ──────────────────────────────────────
//
// Lua KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
// Lua ARGV (5): execution_id, attempt_index, waitpoint_id, waitpoint_key,
//               expires_at

/// Minimal key context for create_pending_waitpoint.
pub struct WaitpointOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
}

ff_function! {
    pub ff_create_pending_waitpoint(args: CreatePendingWaitpointArgs) -> CreatePendingWaitpointResult {
        keys(k: &WaitpointOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.waitpoint(&args.waitpoint_id),                       // 2
            k.idx.pending_waitpoint_expiry(),                          // 3
        }
        argv {
            args.execution_id.to_string(),                             // 1
            args.attempt_index.to_string(),                            // 2
            args.waitpoint_id.to_string(),                             // 3
            args.waitpoint_key.clone(),                                // 4
            {
                let now_ms = TimestampMs::now();
                TimestampMs::from_millis(now_ms.0 + args.expires_in_ms as i64).to_string()
            },                                                         // 5
        }
    }
}

impl FromFcallResult for CreatePendingWaitpointResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(waitpoint_id, waitpoint_key, waitpoint_token)
        let wid = WaitpointId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_create_pending_waitpoint".into(),
                execution_id: None,
                message: format!("bad waitpoint_id: {e}"),
            })?;
        let wkey = r.field_str(1);
        let token = WaitpointToken::new(r.field_str(2));
        Ok(CreatePendingWaitpointResult::Created {
            waitpoint_id: wid,
            waitpoint_key: wkey,
            waitpoint_token: token,
        })
    }
}

// ─── ff_expire_suspension ─────────────────────────────────────────────
//
// Lua KEYS (12): exec_core, suspension_current, waitpoint_hash, wp_condition,
//                attempt_hash, stream_meta, suspension_timeout_zset,
//                suspended_zset, terminal_zset, eligible_zset, delayed_zset,
//                lease_history
// Lua ARGV (1): execution_id

/// Key context for expire_suspension — caller must pre-read current_waitpoint_id
/// and current_attempt_index from exec_core so correct keys can be passed.
pub struct ExpireSuspensionOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
    pub waitpoint_id: &'a WaitpointId,
    pub attempt_index: AttemptIndex,
}

ff_function! {
    pub ff_expire_suspension(args: ExpireSuspensionArgs) -> ExpireSuspensionResult {
        keys(k: &ExpireSuspensionOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.suspension_current(),                                // 2
            k.ctx.waitpoint(k.waitpoint_id),                           // 3
            k.ctx.waitpoint_condition(k.waitpoint_id),                 // 4
            k.ctx.attempt_hash(k.attempt_index),                       // 5
            k.ctx.stream_meta(k.attempt_index),                        // 6
            k.idx.suspension_timeout(),                                // 7
            k.idx.lane_suspended(k.lane_id),                           // 8
            k.idx.lane_terminal(k.lane_id),                            // 9
            k.idx.lane_eligible(k.lane_id),                            // 10
            k.idx.lane_delayed(k.lane_id),                             // 11
            k.ctx.lease_history(),                                     // 12
        }
        argv {
            args.execution_id.to_string(),                             // 1
        }
    }
}

impl FromFcallResult for ExpireSuspensionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(behavior, public_state) or ok("not_found_cleaned") etc.
        let sub = r.field_str(0);
        match sub.as_str() {
            "not_found_cleaned"
            | "not_suspended_cleaned"
            | "no_active_suspension_cleaned"
            | "not_yet_due" => Ok(ExpireSuspensionResult::AlreadySatisfied { reason: sub }),
            "auto_resume" => Ok(ExpireSuspensionResult::Expired {
                behavior_applied: "auto_resume".into(),
            }),
            "escalate" => Ok(ExpireSuspensionResult::Expired {
                behavior_applied: "escalate".into(),
            }),
            _ => Ok(ExpireSuspensionResult::Expired {
                behavior_applied: sub,
            }),
        }
    }
}

// ─── ff_close_waitpoint ───────────────────────────────────────────────
//
// Lua KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
// Lua ARGV (2): waitpoint_id, reason

ff_function! {
    pub ff_close_waitpoint(args: CloseWaitpointArgs) -> CloseWaitpointResult {
        keys(k: &WaitpointOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.waitpoint(&args.waitpoint_id),                       // 2
            k.idx.pending_waitpoint_expiry(),                          // 3
        }
        argv {
            args.waitpoint_id.to_string(),                             // 1
            args.reason.clone(),                                       // 2
        }
    }
}

impl FromFcallResult for CloseWaitpointResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok() or ok("already_closed")
        Ok(CloseWaitpointResult::Closed)
    }
}

// ─── ff_rotate_waitpoint_hmac_secret ──────────────────────────────────
//
// Lua KEYS (1): hmac_secrets
// Lua ARGV (3): new_kid, new_secret_hex, grace_ms

ff_function! {
    /// Rotate the waitpoint HMAC signing kid on a single partition.
    ///
    /// Public FCALL surface for direct-Valkey consumers (e.g. cairn-rs).
    /// ff-server's HTTP rotation endpoint delegates to this same FCALL.
    /// Callers fan out across every partition themselves — this wrapper
    /// touches exactly one.
    ///
    /// Returns [`RotateWaitpointHmacSecretOutcome::Noop`] on exact replay
    /// (same kid + secret). Same kid + DIFFERENT secret surfaces as
    /// [`ScriptError::RotationConflict`] — operator should pick a fresh kid.
    pub ff_rotate_waitpoint_hmac_secret(args: RotateWaitpointHmacSecretArgs) -> RotateWaitpointHmacSecretOutcome {
        keys(k: &IndexKeys) {
            k.waitpoint_hmac_secrets(), // 1
        }
        argv {
            args.new_kid.clone(),        // 1
            args.new_secret_hex.clone(), // 2
            args.grace_ms.to_string(),   // 3
        }
    }
}

impl FromFcallResult for RotateWaitpointHmacSecretOutcome {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua shapes:
        //   ok("rotated", previous_kid_or_empty, new_kid, gc_count)
        //   ok("noop",    kid)
        let variant = r.field_str(0);
        match variant.as_str() {
            "rotated" => {
                let prev = r.field_str(1);
                let new_kid = r.field_str(2);
                let gc_count = r
                    .field_str(3)
                    .parse::<u32>()
                    .map_err(|e| ScriptError::Parse {
                        fcall: "ff_rotate_waitpoint_hmac_secret_outcome".into(),
                        execution_id: None,
                        message: format!("bad gc_count: {e}"),
                    })?;
                Ok(RotateWaitpointHmacSecretOutcome::Rotated {
                    previous_kid: if prev.is_empty() { None } else { Some(prev) },
                    new_kid,
                    gc_count,
                })
            }
            "noop" => Ok(RotateWaitpointHmacSecretOutcome::Noop {
                kid: r.field_str(1),
            }),
            other => Err(ScriptError::Parse {
                fcall: "ff_rotate_waitpoint_hmac_secret_outcome".into(),
                execution_id: None,
                message: format!(
                "unexpected rotation outcome: {other}"
            ),
            }),
        }
    }
}

// ─── ff_list_waitpoint_hmac_kids ──────────────────────────────────────
//
// Lua KEYS (1): hmac_secrets
// Lua ARGV (0): none

ff_function! {
    /// Read-back snapshot of the waitpoint HMAC keystore on one partition.
    /// Callers that need cluster-wide state fan out across partitions.
    pub ff_list_waitpoint_hmac_kids(_args: ListWaitpointHmacKidsArgs) -> WaitpointHmacKids {
        keys(k: &IndexKeys) {
            k.waitpoint_hmac_secrets(), // 1
        }
        argv {}
    }
}

impl FromFcallResult for WaitpointHmacKids {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua shape: ok(current_kid_or_empty, n, kid1, exp1, kid2, exp2, ...)
        let current = r.field_str(0);
        let n = r
            .field_str(1)
            .parse::<usize>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_waitpoint_hmac_kids".into(),
                execution_id: None,
                message: format!("bad verifying count: {e}"),
            })?;
        let mut verifying = Vec::with_capacity(n);
        for i in 0..n {
            let kid = r.field_str(2 + 2 * i);
            let exp = r
                .field_str(2 + 2 * i + 1)
                .parse::<i64>()
                .map_err(|e| ScriptError::Parse {
                    fcall: "ff_waitpoint_hmac_kids".into(),
                    execution_id: None,
                    message: format!("bad expires_at_ms for kid {kid}: {e}"),
                })?;
            verifying.push(VerifyingKid {
                kid,
                expires_at_ms: exp,
            });
        }
        Ok(WaitpointHmacKids {
            current_kid: if current.is_empty() {
                None
            } else {
                Some(current)
            },
            verifying,
        })
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn parse_public_state(s: &str) -> Result<PublicState, ScriptError> {
    match s {
        "waiting" => Ok(PublicState::Waiting),
        "delayed" => Ok(PublicState::Delayed),
        "rate_limited" => Ok(PublicState::RateLimited),
        "waiting_children" => Ok(PublicState::WaitingChildren),
        "active" => Ok(PublicState::Active),
        "suspended" => Ok(PublicState::Suspended),
        "completed" => Ok(PublicState::Completed),
        "failed" => Ok(PublicState::Failed),
        "cancelled" => Ok(PublicState::Cancelled),
        "expired" => Ok(PublicState::Expired),
        "skipped" => Ok(PublicState::Skipped),
        _ => Err(ScriptError::Parse {
            fcall: "ff_waitpoint_hmac_kids".into(),
            execution_id: None,
            message: format!("unknown public_state: {s}"),
        }),
    }
}
