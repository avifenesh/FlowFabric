//! Suspend / deliver_signal / claim_resumed / observe_signals SQL bodies.
//!
//! **Wave 4d follow-up.** Builds on PR #238's primitives:
//!
//! * [`crate::signal::hmac_sign`] / [`crate::signal::hmac_verify`]
//! * [`crate::signal::SERIALIZABLE_RETRY_BUDGET`] +
//!   [`crate::signal::is_retryable_serialization`]
//! * [`crate::suspend::evaluate`] — composite-condition evaluator
//!
//! HMAC signing, retry looping, and composite evaluation are never
//! re-implemented here.
//!
//! # Isolation
//!
//! `suspend` + `deliver_signal` run at SERIALIZABLE with a 3-attempt
//! retry budget (Q11). On retry exhaustion they surface
//! `Contention(RetryExhausted)`. `claim_resumed_execution` +
//! `observe_signals` stay at READ COMMITTED (row-level `FOR UPDATE`
//! for the former, read-only for the latter).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque, ResumeSignal, WaitpointHmac};
use ff_core::contracts::{
    AdditionalWaitpointBinding, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    ClaimedResumedExecution, CompositeBody, DeliverSignalArgs, DeliverSignalResult,
    ListPendingWaitpointsArgs, ListPendingWaitpointsResult, PendingWaitpointInfo, ResumeCondition,
    SignalMatcher, SuspendArgs, SuspendOutcome, SuspendOutcomeDetails, WaitpointBinding,
};
use ff_core::engine_error::{ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::{encode as encode_opaque, HandlePayload};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LeaseEpoch, LeaseFence, SignalId, SuspensionId,
    TimestampMs, WaitpointId,
};
use serde_json::{json, Value as JsonValue};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::lease_event;
use crate::signal::{hmac_sign, hmac_verify, is_retryable_serialization, SERIALIZABLE_RETRY_BUDGET};
use crate::signal_event;
use crate::suspend::evaluate;

// ─── small shared helpers ────────────────────────────────────────────────

fn now_ms() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_millis() as i64).max(0)
}

fn split_exec_id(eid: &ExecutionId) -> Result<(i16, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s.strip_prefix("{fp:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `{{fp:` prefix: {s}"),
    })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    let part: i16 = rest[..close]
        .parse()
        .map_err(|_| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id partition index not u16: {s}"),
        })?;
    let uuid = Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })?;
    Ok((part, uuid))
}

fn decode_handle(handle: &Handle) -> Result<HandlePayload, EngineError> {
    if handle.backend != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!("expected Postgres, got {:?}", handle.backend),
        });
    }
    let decoded = ff_core::handle_codec::decode(&handle.opaque)?;
    if decoded.tag != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!("embedded tag {:?}", decoded.tag),
        });
    }
    Ok(decoded.payload)
}

fn wp_uuid(w: &WaitpointId) -> Result<Uuid, EngineError> {
    Uuid::parse_str(&w.to_string()).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("waitpoint_id not a UUID: {e}"),
    })
}

fn susp_uuid(s: &SuspensionId) -> Result<Uuid, EngineError> {
    Uuid::parse_str(&s.to_string()).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("suspension_id not a UUID: {e}"),
    })
}

/// RFC-020 §3.1.1 — derive `required_signal_names` for a single
/// `waitpoint_key` from the suspend call's `ResumeCondition`. Walks
/// the condition tree and collects every `SignalMatcher::ByName` that
/// targets this specific waitpoint. `Wildcard` matchers contribute
/// nothing (an empty result vec is the wire-level wildcard marker per
/// `PendingWaitpointInfo::required_signal_names` docs).
///
/// `OperatorOnly` / `TimeoutOnly` are rendered with the same sentinel
/// names that the Valkey wire format emits (`__operator_only__` /
/// `__timeout_only__`, see `ff-backend-valkey/src/lib.rs:3205-3222`)
/// so an operator-only or timeout-only waitpoint is distinguishable
/// from a true wildcard at the `PendingWaitpointInfo` surface. The
/// `__`-prefix sentinel values are never real signal names.
///
/// `Count { matcher: None }` returns empty (any signal on the listed
/// waitpoints counts). Duplicates are de-duplicated preserving
/// first-seen order.
fn derive_required_signal_names(cond: &ResumeCondition, wp_key: &str) -> Vec<String> {
    const OPERATOR_ONLY_SENTINEL: &str = "__operator_only__";
    const TIMEOUT_ONLY_SENTINEL: &str = "__timeout_only__";

    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !name.is_empty() && !out.iter().any(|e| e == name) {
            out.push(name.to_owned());
        }
    };
    fn walk(cond: &ResumeCondition, target: &str, push: &mut dyn FnMut(&str)) {
        match cond {
            ResumeCondition::Single {
                waitpoint_key,
                matcher,
            } => {
                if waitpoint_key == target
                    && let SignalMatcher::ByName(name) = matcher
                {
                    push(name.as_str());
                }
            }
            ResumeCondition::OperatorOnly => push(OPERATOR_ONLY_SENTINEL),
            ResumeCondition::TimeoutOnly => push(TIMEOUT_ONLY_SENTINEL),
            ResumeCondition::Composite(body) => walk_body(body, target, push),
            _ => {}
        }
    }
    fn walk_body(body: &CompositeBody, target: &str, push: &mut dyn FnMut(&str)) {
        match body {
            CompositeBody::AllOf { members } => {
                for m in members {
                    walk(m, target, push);
                }
            }
            CompositeBody::Count {
                matcher, waitpoints, ..
            } => {
                if waitpoints.iter().any(|w| w == target)
                    && let Some(SignalMatcher::ByName(name)) = matcher
                {
                    push(name.as_str());
                }
            }
            _ => {}
        }
    }
    walk(cond, wp_key, &mut push);
    out
}

/// RFC-017 §8 / RFC-020 §4.5 — parse the stored `<kid>:<hex>` waitpoint
/// token into a `(token_kid, token_fingerprint)` pair. `token_fingerprint`
/// is the first 16 hex chars (8 bytes) of the HMAC digest — the §8 audit-
/// friendly handle. Malformed input collapses to `("", "")` so callers
/// can skip / log without surfacing a typed error. Mirrors
/// `ff-backend-valkey::parse_waitpoint_token_kid_fp`.
fn parse_waitpoint_token_kid_fp(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((kid, hex)) if !kid.is_empty() && !hex.is_empty() => {
            let fp_len = hex.len().min(16);
            (kid.to_owned(), hex[..fp_len].to_owned())
        }
        _ => (String::new(), String::new()),
    }
}

// ─── SERIALIZABLE retry loop ─────────────────────────────────────────────

/// Return true if an `EngineError::Transport` carries a sqlx
/// serialization-failure SQLSTATE. `map_sqlx_error` stringifies the
/// underlying code; we match defensively on both the raw codes
/// (40001/40P01) and the symbolic labels.
fn is_retryable_engine(err: &EngineError) -> bool {
    match err {
        EngineError::Transport { source, .. } => {
            let s = source.to_string();
            s.contains("40001")
                || s.contains("40P01")
                || s.contains("serialization_failure")
                || s.contains("deadlock_detected")
        }
        // `map_sqlx_error` collapses serialization_failure (40001) +
        // deadlock_detected (40P01) into `Contention(LeaseConflict)`.
        // Inside a SERIALIZABLE retry loop those are retryable; the
        // explicit in-body fence-mismatch case is NOT (a bumped epoch
        // won't unbump on retry). We can't distinguish them by
        // discriminant alone, so the retry loop treats LeaseConflict
        // as retryable — a genuine fence mismatch will still bail
        // after budget exhaustion as RetryExhausted, which callers
        // reconcile by re-reading exec_core.
        EngineError::Contention(ContentionKind::LeaseConflict) => true,
        _ => false,
    }
}

/// Run `op` inside a SERIALIZABLE transaction, retrying up to
/// [`SERIALIZABLE_RETRY_BUDGET`] times on retryable faults. On
/// exhaustion returns `Contention(RetryExhausted)`.
async fn run_serializable<T, F>(pool: &PgPool, mut op: F) -> Result<T, EngineError>
where
    T: Send,
    F: for<'a> FnMut(
            &'a mut Transaction<'_, Postgres>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T, EngineError>> + Send + 'a>,
        > + Send,
{
    for _ in 0..SERIALIZABLE_RETRY_BUDGET {
        let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        let body_res = op(&mut tx).await;
        match body_res {
            Ok(v) => match tx.commit().await {
                Ok(()) => return Ok(v),
                Err(e) if is_retryable_serialization(&e) => continue,
                Err(e) => return Err(map_sqlx_error(e)),
            },
            Err(e) if is_retryable_engine(&e) => {
                let _ = tx.rollback().await;
                continue;
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
    }
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

// ─── dedup outcome (de)serialization ─────────────────────────────────────

fn outcome_to_dedup_json(outcome: &SuspendOutcome) -> JsonValue {
    let details = outcome.details();
    let extras: Vec<JsonValue> = details
        .additional_waitpoints
        .iter()
        .map(|e| {
            json!({
                "waitpoint_id": e.waitpoint_id.to_string(),
                "waitpoint_key": e.waitpoint_key,
                "token": e.waitpoint_token.as_str(),
            })
        })
        .collect();
    let (variant, handle_opaque) = match outcome {
        SuspendOutcome::Suspended { handle, .. } => {
            ("Suspended", Some(hex::encode(handle.opaque.as_bytes())))
        }
        SuspendOutcome::AlreadySatisfied { .. } => ("AlreadySatisfied", None),
        _ => ("Suspended", None),
    };
    json!({
        "variant": variant,
        "details": {
            "suspension_id": details.suspension_id.to_string(),
            "waitpoint_id": details.waitpoint_id.to_string(),
            "waitpoint_key": details.waitpoint_key,
            "token": details.waitpoint_token.as_str(),
            "extras": extras,
        },
        "handle_opaque_hex": handle_opaque,
    })
}

fn outcome_from_dedup_json(v: &JsonValue) -> Result<SuspendOutcome, EngineError> {
    let corrupt = |s: String| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: s,
    };
    let det = &v["details"];
    let suspension_id = SuspensionId::parse(det["suspension_id"].as_str().unwrap_or(""))
        .map_err(|e| corrupt(format!("dedup suspension_id: {e}")))?;
    let waitpoint_id = WaitpointId::parse(det["waitpoint_id"].as_str().unwrap_or(""))
        .map_err(|e| corrupt(format!("dedup waitpoint_id: {e}")))?;
    let waitpoint_key = det["waitpoint_key"].as_str().unwrap_or("").to_owned();
    let token = det["token"].as_str().unwrap_or("").to_owned();
    let mut extras: Vec<AdditionalWaitpointBinding> = Vec::new();
    if let Some(arr) = det["extras"].as_array() {
        for e in arr {
            let wid = WaitpointId::parse(e["waitpoint_id"].as_str().unwrap_or(""))
                .map_err(|err| corrupt(format!("dedup extra wp_id: {err}")))?;
            let wkey = e["waitpoint_key"].as_str().unwrap_or("").to_owned();
            let tok = e["token"].as_str().unwrap_or("").to_owned();
            extras.push(AdditionalWaitpointBinding::new(
                wid,
                wkey,
                WaitpointHmac::new(tok),
            ));
        }
    }
    let details = SuspendOutcomeDetails::new(
        suspension_id,
        waitpoint_id,
        waitpoint_key,
        WaitpointHmac::new(token),
    )
    .with_additional_waitpoints(extras);

    match v["variant"].as_str().unwrap_or("Suspended") {
        "AlreadySatisfied" => Ok(SuspendOutcome::AlreadySatisfied { details }),
        _ => {
            let opaque_hex = v["handle_opaque_hex"].as_str().unwrap_or("");
            let bytes = hex::decode(opaque_hex)
                .map_err(|e| corrupt(format!("dedup handle hex: {e}")))?;
            let opaque = HandleOpaque::new(bytes.into_boxed_slice());
            let handle = Handle::new(BackendTag::Postgres, HandleKind::Suspended, opaque);
            Ok(SuspendOutcome::Suspended { details, handle })
        }
    }
}

// ─── suspend ─────────────────────────────────────────────────────────────

pub(crate) async fn suspend_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    handle: &Handle,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    suspend_core(pool, payload, args).await
}

/// Cairn #322 — service-layer entry point: suspend when the caller
/// holds a lease fence triple but no `Handle`. Resolves `attempt_index`
/// from `ff_attempt` by `(exec_id, attempt_id)` then delegates to the
/// same transactional body used by [`suspend_impl`].
pub(crate) async fn suspend_by_triple_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    exec_id: ExecutionId,
    triple: LeaseFence,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&exec_id)?;
    // Postgres-side attempts are keyed by `(execution_id, attempt_index)`;
    // there is no `attempt_id` column on `ff_attempt`. The triple's
    // `attempt_id` is therefore advisory on this backend — the
    // authoritative "which attempt" pointer lives on `ff_exec_core`.
    // Read it outside the serializable body; `suspend_core` re-fences
    // against `lease_epoch` (`FOR UPDATE`) inside the txn so a racing
    // attempt-bump or lease-bump surfaces as `Contention(LeaseConflict)`.
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT attempt_index FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    let attempt_index_i = match row {
        Some((i,)) => i,
        None => return Err(EngineError::NotFound { entity: "execution" }),
    };
    let attempt_index =
        AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0));

    // Synthesize a HandlePayload — `suspend_core` only reads
    // `execution_id`, `attempt_index`, and `lease_epoch`; the rest of
    // the payload fields are carried through to the replayed-outcome
    // handle encoding (kind = Suspended).
    let payload = HandlePayload::new(
        exec_id,
        attempt_index,
        triple.attempt_id,
        triple.lease_id,
        triple.lease_epoch,
        0,
        ff_core::types::LaneId::new(""),
        ff_core::types::WorkerInstanceId::new(""),
    );
    suspend_core(pool, payload, args).await
}

async fn suspend_core(
    pool: &PgPool,
    payload: HandlePayload,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index_i = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let expected_epoch = payload.lease_epoch.0;
    let idem_key = args.idempotency_key.as_ref().map(|k| k.as_str().to_owned());

    run_serializable(pool, move |tx| {
        let args = args.clone();
        let idem = idem_key.clone();
        let payload = payload.clone();
        Box::pin(async move {
            // 1. Dedup replay check.
            if let Some(key) = idem.as_deref() {
                let row: Option<(JsonValue,)> = sqlx::query_as(
                    "SELECT outcome_json FROM ff_suspend_dedup \
                     WHERE partition_key = $1 AND idempotency_key = $2",
                )
                .bind(part)
                .bind(key)
                .fetch_optional(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;
                if let Some((cached,)) = row {
                    return outcome_from_dedup_json(&cached);
                }
            }

            // 2. Fence check against ff_attempt.
            let epoch_row: Option<(i64,)> = sqlx::query_as(
                "SELECT lease_epoch FROM ff_attempt \
                 WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3 \
                 FOR UPDATE",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
            let observed_epoch: u64 = match epoch_row {
                Some((e,)) => u64::try_from(e).unwrap_or(0),
                None => return Err(EngineError::NotFound { entity: "attempt" }),
            };
            if observed_epoch != expected_epoch {
                return Err(EngineError::Contention(ContentionKind::LeaseConflict));
            }

            // 3. Resolve the active HMAC kid inside the txn.
            let kid_row: Option<(String, Vec<u8>)> = sqlx::query_as(
                "SELECT kid, secret FROM ff_waitpoint_hmac \
                 WHERE active = TRUE \
                 ORDER BY rotated_at_ms DESC LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
            let (kid, secret) = kid_row.ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "ff_waitpoint_hmac empty — rotate a kid before suspend".into(),
            })?;

            // 4. Sign + insert waitpoint_pending for each binding.
            let now = args.now.0;
            let mut signed: Vec<(WaitpointId, String, String)> = Vec::new();
            for binding in args.waitpoints.iter() {
                let (wp_id, wp_key) = match binding {
                    WaitpointBinding::Fresh {
                        waitpoint_id,
                        waitpoint_key,
                    } => (waitpoint_id.clone(), waitpoint_key.clone()),
                    WaitpointBinding::UsePending { waitpoint_id } => {
                        let row: Option<(String,)> = sqlx::query_as(
                            "SELECT waitpoint_key FROM ff_waitpoint_pending \
                             WHERE partition_key = $1 AND waitpoint_id = $2",
                        )
                        .bind(part)
                        .bind(wp_uuid(waitpoint_id)?)
                        .fetch_optional(&mut **tx)
                        .await
                        .map_err(map_sqlx_error)?;
                        let wp_key = row.map(|(k,)| k).unwrap_or_default();
                        (waitpoint_id.clone(), wp_key)
                    }
                    _ => {
                        return Err(EngineError::Validation {
                            kind: ValidationKind::InvalidInput,
                            detail: "unsupported WaitpointBinding variant".into(),
                        });
                    }
                };
                let msg = format!("{}:{}", payload.execution_id, wp_id);
                let token = hmac_sign(&secret, &kid, msg.as_bytes());
                // RFC-020 §3.1.1 — populate 0011 columns on insert.
                // `suspend_core` atomically lands the suspension (exec_core
                // flips to `suspended` in the same txn as this INSERT), so
                // from any observer's perspective the waitpoint is already
                // activated at the moment it becomes visible — there is no
                // separate pending→active transition on Postgres. Write
                // `state = 'active'` + `activated_at_ms = now` directly.
                // `required_signal_names` is derived per-waitpoint from the
                // resume condition; an empty vec denotes the wildcard case
                // per the `PendingWaitpointInfo::required_signal_names`
                // contract.
                let required_names =
                    derive_required_signal_names(&args.resume_condition, &wp_key);
                sqlx::query(
                    "INSERT INTO ff_waitpoint_pending \
                       (partition_key, waitpoint_id, execution_id, token_kid, token, \
                        created_at_ms, expires_at_ms, waitpoint_key, \
                        state, required_signal_names, activated_at_ms) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'active', $9, $6) \
                     ON CONFLICT (partition_key, waitpoint_id) DO UPDATE SET \
                       token_kid = EXCLUDED.token_kid, token = EXCLUDED.token, \
                       waitpoint_key = EXCLUDED.waitpoint_key, \
                       state = EXCLUDED.state, \
                       required_signal_names = EXCLUDED.required_signal_names, \
                       activated_at_ms = EXCLUDED.activated_at_ms",
                )
                .bind(part)
                .bind(wp_uuid(&wp_id)?)
                .bind(exec_uuid)
                .bind(&kid)
                .bind(&token)
                .bind(now)
                .bind(args.timeout_at.map(|t| t.0))
                .bind(&wp_key)
                .bind(&required_names)
                .execute(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;
                signed.push((wp_id, wp_key, token));
            }

            // 5. Insert ff_suspension_current.
            let condition_json =
                serde_json::to_value(&args.resume_condition).map_err(|e| {
                    EngineError::Validation {
                        kind: ValidationKind::Corruption,
                        detail: format!("resume_condition serialize: {e}"),
                    }
                })?;
            sqlx::query(
                "INSERT INTO ff_suspension_current \
                   (partition_key, execution_id, suspension_id, suspended_at_ms, \
                    timeout_at_ms, reason_code, condition, satisfied_set, member_map, \
                    timeout_behavior) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, '[]'::jsonb, '{}'::jsonb, $8) \
                 ON CONFLICT (partition_key, execution_id) DO UPDATE SET \
                   suspension_id = EXCLUDED.suspension_id, \
                   suspended_at_ms = EXCLUDED.suspended_at_ms, \
                   timeout_at_ms = EXCLUDED.timeout_at_ms, \
                   reason_code = EXCLUDED.reason_code, \
                   condition = EXCLUDED.condition, \
                   satisfied_set = '[]'::jsonb, \
                   member_map = '{}'::jsonb, \
                   timeout_behavior = EXCLUDED.timeout_behavior",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(susp_uuid(&args.suspension_id)?)
            .bind(now)
            .bind(args.timeout_at.map(|t| t.0))
            .bind(args.reason_code.as_wire_str())
            .bind(&condition_json)
            .bind(args.timeout_behavior.as_wire_str())
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;

            // 6. Transition exec_core to suspended.
            sqlx::query(
                "UPDATE ff_exec_core \
                    SET lifecycle_phase = 'suspended', \
                        ownership_state = 'released', \
                        eligibility_state = 'not_applicable', \
                        public_state = 'suspended', \
                        attempt_state = 'attempt_interrupted' \
                  WHERE partition_key = $1 AND execution_id = $2",
            )
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;

            // 7. Release + bump lease epoch on ff_attempt.
            sqlx::query(
                "UPDATE ff_attempt \
                    SET worker_id = NULL, \
                        worker_instance_id = NULL, \
                        lease_expires_at_ms = NULL, \
                        lease_epoch = lease_epoch + 1, \
                        outcome = 'attempt_interrupted' \
                  WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;

            // RFC-019 Stage B outbox: lease revoked (suspend).
            lease_event::emit(
                tx,
                part,
                exec_uuid,
                None,
                lease_event::EVENT_REVOKED,
                now,
            )
            .await?;

            // 8. Assemble outcome.
            let (primary_id, primary_key, primary_token) = signed[0].clone();
            let extras: Vec<AdditionalWaitpointBinding> = signed
                .iter()
                .skip(1)
                .map(|(id, key, tok)| {
                    AdditionalWaitpointBinding::new(
                        id.clone(),
                        key.clone(),
                        WaitpointHmac::new(tok.clone()),
                    )
                })
                .collect();
            let details = SuspendOutcomeDetails::new(
                args.suspension_id.clone(),
                primary_id,
                primary_key,
                WaitpointHmac::new(primary_token),
            )
            .with_additional_waitpoints(extras);

            let opaque = encode_opaque(BackendTag::Postgres, &payload);
            let suspended_handle =
                Handle::new(BackendTag::Postgres, HandleKind::Suspended, opaque);
            let outcome = SuspendOutcome::Suspended {
                details,
                handle: suspended_handle,
            };

            // 9. Cache outcome if idempotency key present.
            if let Some(key) = idem.as_deref() {
                let cached = outcome_to_dedup_json(&outcome);
                sqlx::query(
                    "INSERT INTO ff_suspend_dedup \
                       (partition_key, idempotency_key, outcome_json, created_at_ms) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(part)
                .bind(key)
                .bind(&cached)
                .bind(now)
                .execute(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;
            }

            Ok(outcome)
        })
    })
    .await
}

// ─── deliver_signal ──────────────────────────────────────────────────────

pub(crate) async fn deliver_signal_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    args: DeliverSignalArgs,
) -> Result<DeliverSignalResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let wp_u = wp_uuid(&args.waitpoint_id)?;

    run_serializable(pool, move |tx| {
        let args = args.clone();
        Box::pin(async move {
            // 1. Look up pending waitpoint + stored token + kid.
            let row: Option<(String, String, String, Uuid)> = sqlx::query_as(
                "SELECT token_kid, token, waitpoint_key, execution_id \
                   FROM ff_waitpoint_pending \
                  WHERE partition_key = $1 AND waitpoint_id = $2 \
                  FOR UPDATE",
            )
            .bind(part)
            .bind(wp_u)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
            let (kid, stored_token, wp_key, stored_exec) = match row {
                Some(r) => r,
                None => return Err(EngineError::NotFound { entity: "waitpoint" }),
            };
            if stored_exec != exec_uuid {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "waitpoint belongs to a different execution".into(),
                });
            }

            // 2. HMAC verify. Secret comes from the keystore — allows
            //    post-rotation grace verification when kid is inactive
            //    but still stored.
            let secret_row: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT secret FROM ff_waitpoint_hmac WHERE kid = $1",
            )
            .bind(&kid)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
            let (secret,) = secret_row.ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("kid {kid} missing from keystore"),
            })?;
            let presented = args.waitpoint_token.as_str();
            let msg = format!("{}:{}", args.execution_id, args.waitpoint_id);
            hmac_verify(&secret, &kid, msg.as_bytes(), presented).map_err(|e| {
                EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!("waitpoint_token verify: {e}"),
                }
            })?;
            if presented != stored_token {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "waitpoint_token does not match minted token".into(),
                });
            }

            // 3. Load suspension_current (lock the row so concurrent
            //    signals serialize on this execution).
            let susp_row: Option<(JsonValue, JsonValue)> = sqlx::query_as(
                "SELECT condition, member_map FROM ff_suspension_current \
                  WHERE partition_key = $1 AND execution_id = $2 \
                  FOR UPDATE",
            )
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
            let (condition_json, mut member_map) = match susp_row {
                Some(r) => r,
                None => return Err(EngineError::NotFound { entity: "suspension" }),
            };

            // 4. Append signal into member_map[wp_key].
            let signal_blob = json!({
                "signal_id": args.signal_id.to_string(),
                "signal_name": args.signal_name,
                "signal_category": args.signal_category,
                "source_type": args.source_type,
                "source_identity": args.source_identity,
                "correlation_id": args.correlation_id.clone().unwrap_or_default(),
                "accepted_at": args.now.0,
                "payload_hex": args.payload.as_ref().map(hex::encode),
            });
            let map_obj = member_map.as_object_mut().ok_or_else(|| {
                EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: "member_map not a JSON object".into(),
                }
            })?;
            let entry = map_obj.entry(wp_key.clone()).or_insert_with(|| json!([]));
            entry
                .as_array_mut()
                .ok_or_else(|| EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: "member_map[wp_key] not a JSON array".into(),
                })?
                .push(signal_blob);

            // 5. Evaluate composite condition against the updated map.
            let condition: ResumeCondition = serde_json::from_value(condition_json)
                .map_err(|e| EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: format!("condition deserialize: {e}"),
                })?;
            let signals_by_wp: HashMap<String, Vec<ResumeSignal>> = map_obj
                .iter()
                .map(|(k, v)| {
                    let sigs: Vec<ResumeSignal> = v
                        .as_array()
                        .map(|arr| arr.iter().filter_map(resume_signal_from_json).collect())
                        .unwrap_or_default();
                    (k.clone(), sigs)
                })
                .collect();
            let borrowed: HashMap<&str, &[ResumeSignal]> = signals_by_wp
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            let satisfied = evaluate(&condition, &borrowed);

            // 6. Persist member_map (always — both append + satisfy
            //    cases need the updated view for observe_signals).
            sqlx::query(
                "UPDATE ff_suspension_current SET member_map = $1 \
                  WHERE partition_key = $2 AND execution_id = $3",
            )
            .bind(&member_map)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;

            let effect = if satisfied {
                sqlx::query(
                    "UPDATE ff_exec_core \
                        SET public_state = 'resumable', \
                            lifecycle_phase = 'runnable', \
                            eligibility_state = 'eligible_now' \
                      WHERE partition_key = $1 AND execution_id = $2",
                )
                .bind(part)
                .bind(exec_uuid)
                .execute(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;

                sqlx::query(
                    "DELETE FROM ff_waitpoint_pending \
                      WHERE partition_key = $1 AND execution_id = $2",
                )
                .bind(part)
                .bind(exec_uuid)
                .execute(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;

                sqlx::query(
                    "INSERT INTO ff_completion_event \
                       (partition_key, execution_id, outcome, occurred_at_ms) \
                     VALUES ($1, $2, 'resumable', $3)",
                )
                .bind(part)
                .bind(exec_uuid)
                .bind(args.now.0)
                .execute(&mut **tx)
                .await
                .map_err(map_sqlx_error)?;

                "resume_condition_satisfied"
            } else {
                "appended_to_waitpoint"
            };

            // RFC-019 Stage B — `ff_signal_event` outbox. Same tx as
            // the state writes above so the NOTIFY fires iff the
            // delivery commits.
            let wp_id_str = args.waitpoint_id.to_string();
            signal_event::emit(
                tx,
                part,
                exec_uuid,
                &args.signal_id.to_string(),
                Some(wp_id_str.as_str()),
                Some(args.source_identity.as_str()),
                args.now.0,
            )
            .await?;

            Ok(DeliverSignalResult::Accepted {
                signal_id: args.signal_id.clone(),
                effect: effect.to_owned(),
            })
        })
    })
    .await
}

fn resume_signal_from_json(v: &JsonValue) -> Option<ResumeSignal> {
    let signal_id = SignalId::parse(v["signal_id"].as_str()?).ok()?;
    Some(ResumeSignal {
        signal_id,
        signal_name: v["signal_name"].as_str()?.to_owned(),
        signal_category: v["signal_category"].as_str().unwrap_or("").to_owned(),
        source_type: v["source_type"].as_str().unwrap_or("").to_owned(),
        source_identity: v["source_identity"].as_str().unwrap_or("").to_owned(),
        correlation_id: v["correlation_id"].as_str().unwrap_or("").to_owned(),
        accepted_at: TimestampMs::from_millis(v["accepted_at"].as_i64().unwrap_or(0)),
        payload: v["payload_hex"].as_str().and_then(|h| hex::decode(h).ok()),
    })
}

// ─── claim_resumed_execution ─────────────────────────────────────────────

pub(crate) async fn claim_resumed_execution_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    args: ClaimResumedExecutionArgs,
) -> Result<ClaimResumedExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let row: Option<(String, i32)> = sqlx::query_as(
        "SELECT public_state, attempt_index FROM ff_exec_core \
          WHERE partition_key = $1 AND execution_id = $2 \
          FOR UPDATE",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let (public_state, attempt_index_i) = match row {
        Some(r) => r,
        None => {
            tx.rollback().await.ok();
            return Err(EngineError::NotFound { entity: "execution" });
        }
    };
    if public_state != "resumable" {
        tx.rollback().await.ok();
        return Err(EngineError::Contention(
            ContentionKind::NotAResumedExecution,
        ));
    }

    let now = now_ms();
    let lease_ttl = i64::try_from(args.lease_ttl_ms).unwrap_or(0);
    let new_expires = now.saturating_add(lease_ttl);

    sqlx::query(
        "UPDATE ff_attempt \
            SET worker_id = $1, worker_instance_id = $2, \
                lease_epoch = lease_epoch + 1, \
                lease_expires_at_ms = $3, started_at_ms = $4, outcome = NULL \
          WHERE partition_key = $5 AND execution_id = $6 AND attempt_index = $7",
    )
    .bind(args.worker_id.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(new_expires)
    .bind(now)
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // #356: started_at_ms is set-once on ff_exec_core; the resume-claim
    // path preserves the original first-claim timestamp via COALESCE.
    // On a suspended exec that was resumed before 0016 backfill, the
    // column may be NULL here — we seed it with `now` as a defensible
    // fallback (post-suspension resume is as close to "first observable
    // claim" as the column can get in that edge case).
    sqlx::query(
        "UPDATE ff_exec_core \
            SET lifecycle_phase = 'active', ownership_state = 'leased', \
                eligibility_state = 'not_applicable', \
                public_state = 'running', attempt_state = 'running_attempt', \
                started_at_ms = COALESCE(started_at_ms, $3) \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let epoch_row: (i64,) = sqlx::query_as(
        "SELECT lease_epoch FROM ff_attempt \
          WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease acquired (claim_resumed_execution).
    let lease_id_str = args.lease_id.to_string();
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        Some(&lease_id_str),
        lease_event::EVENT_ACQUIRED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let attempt_index = AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0));
    let lease_epoch = LeaseEpoch(u64::try_from(epoch_row.0).unwrap_or(0));
    let attempt_id = AttemptId::new();

    Ok(ClaimResumedExecutionResult::Claimed(
        ClaimedResumedExecution {
            execution_id: args.execution_id.clone(),
            lease_id: args.lease_id.clone(),
            lease_epoch,
            attempt_index,
            attempt_id,
            lease_expires_at: TimestampMs::from_millis(new_expires),
        },
    ))
}

// ─── observe_signals ─────────────────────────────────────────────────────

pub(crate) async fn observe_signals_impl(
    pool: &PgPool,
    handle: &Handle,
) -> Result<Vec<ResumeSignal>, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;

    let row: Option<(JsonValue,)> = sqlx::query_as(
        "SELECT member_map FROM ff_suspension_current \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some((member_map,)) = row else {
        return Ok(Vec::new());
    };
    let mut out: Vec<ResumeSignal> = Vec::new();
    if let Some(map) = member_map.as_object() {
        for (_wp_key, arr) in map {
            if let Some(sigs) = arr.as_array() {
                for v in sigs {
                    if let Some(s) = resume_signal_from_json(v) {
                        out.push(s);
                    }
                }
            }
        }
    }
    Ok(out)
}

// ─── list_pending_waitpoints ─────────────────────────────────────────────

/// RFC-020 §4.5 — read-only projection of pending-or-active waitpoints
/// for a given execution. SQL parity with Valkey's SSCAN + 2× HMGET
/// shape: single-table scan of `ff_waitpoint_pending` with cursor
/// `(waitpoint_id > $after ORDER BY waitpoint_id LIMIT $limit+1)`,
/// surfaces the `PendingWaitpointInfo` 10-field contract.
/// `token_fingerprint` is computed from the stored `<kid>:<hex>` token
/// — the raw token never crosses the trait boundary per RFC-017 Stage
/// D1 / §8.
///
/// Pre-read existence check on `ff_exec_core` mirrors Valkey's
/// `EXISTS exec_core` so a non-existent execution surfaces `NotFound`
/// rather than an empty page.
///
/// Rows with `state NOT IN ('pending', 'active')` (e.g. `'closed'`)
/// are filtered server-side to match Valkey's client-side keep-filter.
pub(crate) async fn list_pending_waitpoints_impl(
    pool: &PgPool,
    args: ListPendingWaitpointsArgs,
) -> Result<ListPendingWaitpointsResult, EngineError> {
    const DEFAULT_LIMIT: u32 = 100;
    const MAX_LIMIT: u32 = 1000;

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
    let after_uuid = match args.after.as_ref() {
        Some(wp) => Some(wp_uuid(wp)?),
        None => None,
    };

    // Existence probe — a non-existent execution is `NotFound`, not an
    // empty page. Matches Valkey's `EXISTS exec_core` pre-check.
    let exists: Option<(i16,)> = sqlx::query_as(
        "SELECT 1::smallint FROM ff_exec_core \
          WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    if exists.is_none() {
        return Err(EngineError::NotFound { entity: "execution" });
    }

    // Page: request `limit + 1` so we can detect "more to come" without
    // a second round-trip. Row tuple order matches the SELECT below:
    // (waitpoint_id, waitpoint_key, state, required_signal_names,
    //  created_at_ms, activated_at_ms, expires_at_ms, token_kid, token).
    // Raw token is fingerprinted client-side — never returned across
    // the trait boundary per RFC-017 Stage D1 / §8.
    type Row = (
        Uuid,
        String,
        String,
        Vec<String>,
        i64,
        Option<i64>,
        Option<i64>,
        String,
        String,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT waitpoint_id, waitpoint_key, state, required_signal_names, \
                created_at_ms, activated_at_ms, expires_at_ms, token_kid, token \
           FROM ff_waitpoint_pending \
          WHERE partition_key = $1 \
            AND execution_id = $2 \
            AND state IN ('pending', 'active') \
            AND ($3::uuid IS NULL OR waitpoint_id > $3) \
          ORDER BY waitpoint_id \
          LIMIT $4",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(after_uuid)
    .bind(limit + 1)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let has_more = rows.len() as i64 > limit;
    let take_n = if has_more { limit as usize } else { rows.len() };

    let mut entries: Vec<PendingWaitpointInfo> = Vec::with_capacity(take_n);
    for (wp_uid, wp_key, state, req_names, created_ms, activated_ms, expires_ms, _kid, token)
        in rows.into_iter().take(take_n)
    {
        let wp_id = WaitpointId::from_uuid(wp_uid);
        // Parse stored `<kid>:<hex>` into audit-safe pair. The `token_kid`
        // column is redundant with the parsed kid — we prefer the parsed
        // one so the surface stays byte-identical to Valkey's.
        let (token_kid, token_fingerprint) = parse_waitpoint_token_kid_fp(&token);
        let mut info = PendingWaitpointInfo::new(
            wp_id,
            wp_key,
            state,
            TimestampMs(created_ms),
            args.execution_id.clone(),
            token_kid,
            token_fingerprint,
        );
        if !req_names.is_empty() {
            info = info.with_required_signal_names(req_names);
        }
        if let Some(ms) = activated_ms {
            info = info.with_activated_at(TimestampMs(ms));
        }
        if let Some(ms) = expires_ms {
            info = info.with_expires_at(TimestampMs(ms));
        }
        entries.push(info);
    }

    let next_cursor = if has_more {
        entries.last().map(|e| e.waitpoint_id.clone())
    } else {
        None
    };
    let mut result = ListPendingWaitpointsResult::new(entries);
    if let Some(cursor) = next_cursor {
        result = result.with_next_cursor(cursor);
    }
    Ok(result)
}
