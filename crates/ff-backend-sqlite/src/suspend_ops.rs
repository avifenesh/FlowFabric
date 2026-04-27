//! Suspend + signal + waitpoint bodies for the SQLite backend.
//!
//! RFC-023 Phase 2b.2.1 (Group B producer). Parallels
//! `ff-backend-postgres/src/suspend_ops.rs` statement-by-statement
//! with SQLite dialect deltas:
//!
//!   * `BEGIN IMMEDIATE` single-writer lock replaces PG's SERIALIZABLE
//!     isolation + row-level `FOR UPDATE`.
//!   * `jsonb` columns become TEXT — JSON codec is serde_json at the
//!     Rust boundary.
//!   * `text[]` columns (`required_signal_names`) become TEXT holding a
//!     JSON array literal.
//!
//! The composite-condition evaluator is the pure-Rust
//! [`evaluator::evaluate`] helper below — a copy of the PG-side
//! evaluator at `ff-backend-postgres/src/suspend.rs` ported to the
//! SQLite module tree. Evaluator logic is backend-agnostic; the copy
//! avoids a cross-backend crate dependency.

use std::collections::HashMap;

use ff_core::backend::{
    BackendTag, Handle, HandleKind, HandleOpaque, PendingWaitpoint, ResumeSignal, WaitpointHmac,
};
use ff_core::contracts::{
    AdditionalWaitpointBinding, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    ClaimedResumedExecution, DeliverSignalArgs, DeliverSignalResult, ResumeCondition,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllEntry,
    RotateWaitpointHmacSecretAllResult, RotateWaitpointHmacSecretOutcome, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SuspendArgs, SuspendOutcome, SuspendOutcomeDetails,
    WaitpointBinding,
};
use ff_core::crypto::hmac::{hmac_sign, hmac_verify};
use ff_core::engine_error::{ConflictKind, ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::{encode as encode_opaque, HandlePayload};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LeaseEpoch, LeaseFence, SignalId, SuspensionId,
    TimestampMs, WaitpointId,
};
use serde_json::{json, Value as JsonValue};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::errors::map_sqlx_error;
use crate::handle_codec::decode_handle;
use crate::pubsub::{OutboxEvent, PubSub};
use crate::queries::{signal as q_signal, suspend as q_suspend, waitpoint as q_wp};

// ── shared small helpers ──────────────────────────────────────────────

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

fn split_exec_id(eid: &ExecutionId) -> Result<(i64, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s
        .strip_prefix("{fp:")
        .ok_or_else(|| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id missing `{{fp:` prefix: {s}"),
        })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    let part: i64 = rest[..close]
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

async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, EngineError> {
    let mut conn = pool.acquire().await.map_err(map_sqlx_error)?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(conn)
}

async fn commit_or_rollback(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
) -> Result<(), EngineError> {
    if let Err(e) = sqlx::query("COMMIT")
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)
    {
        let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
        return Err(e);
    }
    Ok(())
}

async fn rollback_quiet(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
}

// ── RFC-020 §3.1.1 — derive required_signal_names (mirror PG) ─────────

fn derive_required_signal_names(cond: &ResumeCondition, wp_key: &str) -> Vec<String> {
    use ff_core::contracts::{CompositeBody, SignalMatcher};
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

// ── Composite-condition evaluator (parity with PG `suspend::evaluate`) ─

mod evaluator {
    use super::*;
    use ff_core::contracts::{CompositeBody, CountKind, SignalMatcher};
    use std::collections::HashSet;

    pub type SignalsByWaitpoint<'a> = HashMap<&'a str, &'a [ResumeSignal]>;

    pub fn evaluate(condition: &ResumeCondition, by_wp: &SignalsByWaitpoint<'_>) -> bool {
        match condition {
            ResumeCondition::Single {
                waitpoint_key,
                matcher,
            } => by_wp
                .get(waitpoint_key.as_str())
                .map(|sigs| sigs.iter().any(|s| matcher_matches(matcher, s)))
                .unwrap_or(false),
            ResumeCondition::OperatorOnly | ResumeCondition::TimeoutOnly => false,
            ResumeCondition::Composite(body) => evaluate_composite(body, by_wp),
            _ => false,
        }
    }

    fn evaluate_composite(body: &CompositeBody, by_wp: &SignalsByWaitpoint<'_>) -> bool {
        match body {
            CompositeBody::AllOf { members } => {
                !members.is_empty() && members.iter().all(|m| evaluate(m, by_wp))
            }
            CompositeBody::Count {
                n,
                count_kind,
                matcher,
                waitpoints,
            } => {
                let mut candidates: Vec<(&str, &ResumeSignal)> = Vec::new();
                for wpk in waitpoints {
                    let Some(sigs) = by_wp.get(wpk.as_str()) else {
                        continue;
                    };
                    for s in sigs.iter() {
                        if matcher
                            .as_ref()
                            .map(|m| matcher_matches(m, s))
                            .unwrap_or(true)
                        {
                            candidates.push((wpk.as_str(), s));
                        }
                    }
                }
                let distinct_count = match count_kind {
                    CountKind::DistinctWaitpoints => {
                        let mut set: HashSet<&str> = HashSet::new();
                        for (wpk, _) in &candidates {
                            set.insert(wpk);
                        }
                        set.len() as u32
                    }
                    CountKind::DistinctSignals => {
                        let mut set: HashSet<String> = HashSet::new();
                        for (_, s) in &candidates {
                            set.insert(s.signal_id.0.to_string());
                        }
                        set.len() as u32
                    }
                    CountKind::DistinctSources => {
                        let mut set: HashSet<(&str, &str)> = HashSet::new();
                        for (_, s) in &candidates {
                            set.insert((s.source_type.as_str(), s.source_identity.as_str()));
                        }
                        set.len() as u32
                    }
                    _ => 0,
                };
                distinct_count >= *n
            }
            _ => false,
        }
    }

    fn matcher_matches(matcher: &SignalMatcher, signal: &ResumeSignal) -> bool {
        match matcher {
            SignalMatcher::ByName(name) => signal.signal_name.as_str() == name.as_str(),
            SignalMatcher::Wildcard => true,
            _ => false,
        }
    }
}

// ── dedup outcome (de)serialization — mirror PG shape ─────────────────

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
            let handle = Handle::new(BackendTag::Sqlite, HandleKind::Suspended, opaque);
            Ok(SuspendOutcome::Suspended { details, handle })
        }
    }
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

// ── suspend core ──────────────────────────────────────────────────────

pub(crate) async fn suspend_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    handle: &Handle,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    suspend_core(pool, pubsub, payload, args).await
}

pub(crate) async fn suspend_by_triple_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    exec_id: ExecutionId,
    triple: LeaseFence,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&exec_id)?;
    // SQLite stores attempt_index on ff_exec_core; reconstruct an
    // ephemeral payload then delegate. `suspend_core` re-fences on
    // `lease_epoch` under BEGIN IMMEDIATE.
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT attempt_index FROM ff_exec_core \
          WHERE partition_key = ?1 AND execution_id = ?2",
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
    suspend_core(pool, pubsub, payload, args).await
}

async fn suspend_core(
    pool: &SqlitePool,
    pubsub: &PubSub,
    payload: HandlePayload,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index_i = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;
    let idem_key = args.idempotency_key.as_ref().map(|k| k.as_str().to_owned());

    let mut conn = begin_immediate(pool).await?;
    let result = suspend_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index_i,
        expected_epoch,
        payload,
        args,
        idem_key,
    )
    .await;
    match result {
        Ok((outcome, _emits)) => {
            commit_or_rollback(&mut conn).await?;
            // Suspend emits no wakeup channels — the waitpoint row is
            // written, suspension_current is populated, but nothing on
            // the RFC-023 §4.2 subscriber surface observes a plain
            // suspension. Signal delivery is the event that fires
            // `signal_delivery` / `completion` downstream.
            let _ = pubsub;
            Ok(outcome)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn suspend_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index_i: i64,
    expected_epoch: u64,
    payload: HandlePayload,
    args: SuspendArgs,
    idem_key: Option<String>,
) -> Result<(SuspendOutcome, ()), EngineError> {
    // 1. Dedup replay.
    if let Some(key) = idem_key.as_deref() {
        let row: Option<(String,)> = sqlx::query_as(q_suspend::SELECT_SUSPEND_DEDUP_SQL)
            .bind(part)
            .bind(key)
            .fetch_optional(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        if let Some((text,)) = row {
            let cached: JsonValue =
                serde_json::from_str(&text).map_err(|e| EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: format!("suspend_dedup outcome_json: {e}"),
                })?;
            return Ok((outcome_from_dedup_json(&cached)?, ()));
        }
    }

    // 2. Fence check.
    let epoch_row = sqlx::query(q_suspend::SELECT_ATTEMPT_EPOCH_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let observed_epoch: u64 = match epoch_row {
        Some(r) => {
            let e: i64 = r.try_get("lease_epoch").map_err(map_sqlx_error)?;
            u64::try_from(e).unwrap_or(0)
        }
        None => return Err(EngineError::NotFound { entity: "attempt" }),
    };
    if observed_epoch != expected_epoch {
        return Err(EngineError::Contention(ContentionKind::LeaseConflict));
    }

    // 3. Active HMAC kid from keystore.
    let kid_row = sqlx::query(q_wp::SELECT_ACTIVE_HMAC_SQL)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let (kid, secret): (String, Vec<u8>) = match kid_row {
        Some(r) => (
            r.try_get("kid").map_err(map_sqlx_error)?,
            r.try_get("secret").map_err(map_sqlx_error)?,
        ),
        None => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "ff_waitpoint_hmac empty — seed a kid before suspend".into(),
            });
        }
    };

    // 4. Mint HMAC tokens and INSERT pending waitpoints.
    let now = args.now.0;
    let mut signed: Vec<(WaitpointId, String, String)> = Vec::new();
    for binding in args.waitpoints.iter() {
        let (wp_id, wp_key) = match binding {
            WaitpointBinding::Fresh {
                waitpoint_id,
                waitpoint_key,
            } => (waitpoint_id.clone(), waitpoint_key.clone()),
            WaitpointBinding::UsePending { waitpoint_id } => {
                let row: Option<(String,)> =
                    sqlx::query_as(q_wp::SELECT_WAITPOINT_KEY_BY_ID_SQL)
                        .bind(part)
                        .bind(wp_uuid(waitpoint_id)?)
                        .fetch_optional(&mut **conn)
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
        let required_names = derive_required_signal_names(&args.resume_condition, &wp_key);
        let required_names_json = serde_json::to_string(&required_names).unwrap_or_else(|_| "[]".into());
        sqlx::query(q_wp::UPSERT_WAITPOINT_PENDING_ACTIVE_SQL)
            .bind(part)
            .bind(wp_uuid(&wp_id)?)
            .bind(exec_uuid)
            .bind(&kid)
            .bind(&token)
            .bind(now)
            .bind(args.timeout_at.map(|t| t.0))
            .bind(&wp_key)
            .bind(&required_names_json)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        signed.push((wp_id, wp_key, token));
    }

    // 5. UPSERT ff_suspension_current.
    let condition_json = serde_json::to_string(&args.resume_condition).map_err(|e| {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("resume_condition serialize: {e}"),
        }
    })?;
    sqlx::query(q_suspend::UPSERT_SUSPENSION_CURRENT_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(susp_uuid(&args.suspension_id)?)
        .bind(now)
        .bind(args.timeout_at.map(|t| t.0))
        .bind(args.reason_code.as_wire_str())
        .bind(&condition_json)
        .bind(args.timeout_behavior.as_wire_str())
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 6. Transition exec_core to suspended.
    sqlx::query(q_suspend::UPDATE_EXEC_CORE_SUSPEND_SQL)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 7. Release + bump lease on ff_attempt.
    sqlx::query(q_suspend::UPDATE_ATTEMPT_SUSPEND_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

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

    let opaque = encode_opaque(BackendTag::Sqlite, &payload);
    let suspended_handle = Handle::new(BackendTag::Sqlite, HandleKind::Suspended, opaque);
    let outcome = SuspendOutcome::Suspended {
        details,
        handle: suspended_handle,
    };

    // 9. Cache outcome for idempotent replay.
    if let Some(key) = idem_key.as_deref() {
        let cached = outcome_to_dedup_json(&outcome);
        let cached_text = cached.to_string();
        sqlx::query(q_suspend::INSERT_SUSPEND_DEDUP_SQL)
            .bind(part)
            .bind(key)
            .bind(&cached_text)
            .bind(now)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
    }

    Ok((outcome, ()))
}

// ── create_waitpoint ─────────────────────────────────────────────────

pub(crate) async fn create_waitpoint_impl(
    pool: &SqlitePool,
    handle: &Handle,
    waitpoint_key: &str,
    expires_in: std::time::Duration,
) -> Result<PendingWaitpoint, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;

    let mut conn = begin_immediate(pool).await?;
    let result = async {
        // Active kid lookup.
        let kid_row = sqlx::query(q_wp::SELECT_ACTIVE_HMAC_SQL)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let (kid, secret): (String, Vec<u8>) = match kid_row {
            Some(r) => (
                r.try_get("kid").map_err(map_sqlx_error)?,
                r.try_get("secret").map_err(map_sqlx_error)?,
            ),
            None => {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "ff_waitpoint_hmac empty — seed a kid before create_waitpoint".into(),
                });
            }
        };

        let wp_id = WaitpointId::new();
        let wp_u = wp_uuid(&wp_id)?;
        let now = now_ms();
        let expires_at = now.saturating_add(i64::try_from(expires_in.as_millis()).unwrap_or(i64::MAX));
        let msg = format!("{}:{}", payload.execution_id, wp_id);
        let token = hmac_sign(&secret, &kid, msg.as_bytes());

        sqlx::query(q_wp::INSERT_WAITPOINT_PENDING_SQL)
            .bind(part)
            .bind(wp_u)
            .bind(exec_uuid)
            .bind(&kid)
            .bind(&token)
            .bind(now)
            .bind(expires_at)
            .bind(waitpoint_key)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        Ok::<_, EngineError>(PendingWaitpoint::new(wp_id, WaitpointHmac::new(token)))
    }
    .await;
    match result {
        Ok(r) => {
            commit_or_rollback(&mut conn).await?;
            Ok(r)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

// ── observe_signals ──────────────────────────────────────────────────

pub(crate) async fn observe_signals_impl(
    pool: &SqlitePool,
    handle: &Handle,
) -> Result<Vec<ResumeSignal>, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;

    let row: Option<(String,)> = sqlx::query_as(
        "SELECT member_map FROM ff_suspension_current \
          WHERE partition_key = ?1 AND execution_id = ?2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    let Some((member_map_text,)) = row else {
        return Ok(Vec::new());
    };
    let member_map: JsonValue =
        serde_json::from_str(&member_map_text).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("member_map: {e}"),
        })?;
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

// ── deliver_signal ───────────────────────────────────────────────────

pub(crate) async fn deliver_signal_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: DeliverSignalArgs,
) -> Result<DeliverSignalResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let wp_u = wp_uuid(&args.waitpoint_id)?;

    let mut conn = begin_immediate(pool).await?;
    let outcome = deliver_signal_inner(&mut conn, part, exec_uuid, wp_u, &args).await;
    match outcome {
        Ok((result, emits)) => {
            commit_or_rollback(&mut conn).await?;
            // Post-commit broadcast emit per RFC-023 §4.2.
            for (channel, ev) in emits {
                let sender = match channel {
                    SignalEmit::SignalDelivery => &pubsub.signal_delivery,
                    SignalEmit::Completion => &pubsub.completion,
                };
                PubSub::emit(sender, ev);
            }
            Ok(result)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[derive(Clone, Copy)]
enum SignalEmit {
    SignalDelivery,
    Completion,
}

async fn deliver_signal_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    wp_u: Uuid,
    args: &DeliverSignalArgs,
) -> Result<(DeliverSignalResult, Vec<(SignalEmit, OutboxEvent)>), EngineError> {
    // 1. Lookup pending waitpoint row.
    let wp_row = sqlx::query(q_wp::SELECT_WAITPOINT_FOR_DELIVER_SQL)
        .bind(part)
        .bind(wp_u)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let wp_row = match wp_row {
        Some(r) => r,
        None => return Err(EngineError::NotFound { entity: "waitpoint" }),
    };
    let kid: String = wp_row.try_get("token_kid").map_err(map_sqlx_error)?;
    let stored_token: String = wp_row.try_get("token").map_err(map_sqlx_error)?;
    let wp_key: String = wp_row.try_get("waitpoint_key").map_err(map_sqlx_error)?;
    let stored_exec: Uuid = wp_row.try_get("execution_id").map_err(map_sqlx_error)?;
    if stored_exec != exec_uuid {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "waitpoint belongs to a different execution".into(),
        });
    }

    // 2. HMAC verify (secret from keystore).
    let secret_row: Option<(Vec<u8>,)> = sqlx::query_as(q_wp::SELECT_HMAC_SECRET_BY_KID_SQL)
        .bind(&kid)
        .fetch_optional(&mut **conn)
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

    // 3. Load suspension row.
    let susp_row: Option<(String, String)> =
        sqlx::query_as(q_suspend::SELECT_SUSPENSION_CONDITION_AND_MAP_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
    let (condition_text, mut member_map_text) = match susp_row {
        Some(r) => r,
        None => return Err(EngineError::NotFound { entity: "suspension" }),
    };
    let mut member_map: JsonValue =
        serde_json::from_str(&member_map_text).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("member_map: {e}"),
        })?;
    let _ = &mut member_map_text;

    // 4. Append signal blob.
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
    let map_obj = member_map.as_object_mut().ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: "member_map not a JSON object".into(),
    })?;
    let entry = map_obj.entry(wp_key.clone()).or_insert_with(|| json!([]));
    entry
        .as_array_mut()
        .ok_or_else(|| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: "member_map[wp_key] not a JSON array".into(),
        })?
        .push(signal_blob);

    // 5. Evaluate condition.
    let condition: ResumeCondition =
        serde_json::from_str(&condition_text).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("condition: {e}"),
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
    let satisfied = evaluator::evaluate(&condition, &borrowed);

    // 6. Persist member_map.
    let new_map_text = member_map.to_string();
    sqlx::query(q_suspend::UPDATE_SUSPENSION_MEMBER_MAP_SQL)
        .bind(&new_map_text)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    let mut emits: Vec<(SignalEmit, OutboxEvent)> = Vec::new();

    let effect = if satisfied {
        sqlx::query(q_suspend::UPDATE_EXEC_CORE_RESUMABLE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(q_wp::DELETE_WAITPOINTS_BY_EXEC_SQL)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(q_signal::INSERT_COMPLETION_RESUMABLE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(args.now.0)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        let completion_event_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
            .fetch_one(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        emits.push((
            SignalEmit::Completion,
            OutboxEvent {
                event_id: completion_event_id,
                partition_key: part,
            },
        ));
        "resume_condition_satisfied"
    } else {
        "appended_to_waitpoint"
    };

    // 7. Signal-event outbox row.
    sqlx::query(q_signal::INSERT_SIGNAL_EVENT_SQL)
        .bind(exec_uuid.to_string())
        .bind(args.signal_id.to_string())
        .bind(Some(args.waitpoint_id.to_string()))
        .bind(Some(args.source_identity.as_str()))
        .bind(args.now.0)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let sig_event_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    emits.push((
        SignalEmit::SignalDelivery,
        OutboxEvent {
            event_id: sig_event_id,
            partition_key: part,
        },
    ));

    Ok((
        DeliverSignalResult::Accepted {
            signal_id: args.signal_id.clone(),
            effect: effect.to_owned(),
        },
        emits,
    ))
}

// ── claim_resumed_execution ──────────────────────────────────────────

pub(crate) async fn claim_resumed_execution_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: ClaimResumedExecutionArgs,
) -> Result<ClaimResumedExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let row = sqlx::query(q_suspend::SELECT_EXEC_STATE_FOR_RESUME_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let row = row.ok_or(EngineError::NotFound { entity: "execution" })?;
        let public_state: String = row.try_get("public_state").map_err(map_sqlx_error)?;
        let attempt_index_i: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
        if public_state != "resumable" {
            return Err(EngineError::Contention(ContentionKind::NotAResumedExecution));
        }

        let now = now_ms();
        let lease_ttl = i64::try_from(args.lease_ttl_ms).unwrap_or(0);
        let new_expires = now.saturating_add(lease_ttl);

        sqlx::query(q_suspend::UPDATE_ATTEMPT_CLAIM_RESUMED_SQL)
            .bind(args.worker_id.as_str())
            .bind(args.worker_instance_id.as_str())
            .bind(new_expires)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_suspend::UPDATE_EXEC_CORE_RUNNING_SQL)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let epoch_row = sqlx::query(q_suspend::SELECT_ATTEMPT_LEASE_EPOCH_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let epoch_i: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;

        // RFC-019 Stage B outbox: lease acquired on resume. Mirrors PG
        // reference at suspend_ops.rs:981-990.
        sqlx::query(crate::queries::dispatch::INSERT_LEASE_EVENT_SQL)
            .bind(exec_uuid.to_string())
            .bind("acquired")
            .bind(now)
            .bind(part)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let lease_event_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let attempt_index =
            AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0));
        let lease_epoch = LeaseEpoch(u64::try_from(epoch_i).unwrap_or(0));
        let attempt_id = AttemptId::new();

        Ok::<_, EngineError>((
            ClaimResumedExecutionResult::Claimed(ClaimedResumedExecution {
                execution_id: args.execution_id.clone(),
                lease_id: args.lease_id.clone(),
                lease_epoch,
                attempt_index,
                attempt_id,
                lease_expires_at: TimestampMs::from_millis(new_expires),
            }),
            lease_event_id,
        ))
    }
    .await;

    match result {
        Ok((out, lease_event_id)) => {
            commit_or_rollback(&mut conn).await?;
            // Post-commit broadcast: lease-history (acquired).
            PubSub::emit(
                &pubsub.lease_history,
                OutboxEvent {
                    event_id: lease_event_id,
                    partition_key: part,
                },
            );
            Ok(out)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

// ── HMAC secret management ───────────────────────────────────────────

pub(crate) async fn seed_waitpoint_hmac_secret_impl(
    pool: &SqlitePool,
    args: SeedWaitpointHmacSecretArgs,
) -> Result<SeedOutcome, EngineError> {
    if args.secret_hex.len() != 64 || !args.secret_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "secret_hex must be 64 hex characters (256-bit secret)".into(),
        });
    }
    if args.kid.is_empty() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "kid must be non-empty".into(),
        });
    }
    let secret_bytes = hex::decode(&args.secret_hex).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: "secret_hex is not valid hex".into(),
    })?;

    let mut conn = begin_immediate(pool).await?;

    let work = async {
        let existing: Option<(Vec<u8>,)> =
            sqlx::query_as(q_wp::SELECT_HMAC_SECRET_BY_KID_SQL)
                .bind(&args.kid)
                .fetch_optional(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        if let Some((prior,)) = existing {
            return Ok(SeedOutcome::AlreadySeeded {
                kid: args.kid.clone(),
                same_secret: prior == secret_bytes,
            });
        }

        let active: Option<(String,)> = sqlx::query_as(q_wp::SELECT_ACTIVE_KID_SQL)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        if let Some((active_kid,)) = active {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "seed_waitpoint_hmac_secret: a different kid {active_kid:?} is already active; \
                     use rotate_waitpoint_hmac_secret_all to change kid"
                ),
            });
        }

        sqlx::query(q_wp::INSERT_HMAC_ROW_SQL)
            .bind(&args.kid)
            .bind(&secret_bytes)
            .bind(now_ms())
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        Ok::<_, EngineError>(SeedOutcome::Seeded { kid: args.kid.clone() })
    }
    .await;

    match work {
        Ok(o) => {
            commit_or_rollback(&mut conn).await?;
            Ok(o)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

pub(crate) async fn rotate_waitpoint_hmac_secret_all_impl(
    pool: &SqlitePool,
    args: RotateWaitpointHmacSecretAllArgs,
) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
    let secret_bytes = match hex::decode(&args.new_secret_hex) {
        Ok(b) => b,
        Err(_) => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "new_secret_hex is not valid hex".into(),
            });
        }
    };
    let now = now_ms();

    let mut conn = begin_immediate(pool).await?;
    let outcome_res: Result<RotateWaitpointHmacSecretOutcome, EngineError> = async {
        let existing: Option<(Vec<u8>,)> = sqlx::query_as(q_wp::SELECT_HMAC_SECRET_BY_KID_SQL)
            .bind(&args.new_kid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        if let Some((prior,)) = existing {
            if prior == secret_bytes {
                return Ok(RotateWaitpointHmacSecretOutcome::Noop {
                    kid: args.new_kid.clone(),
                });
            }
            return Err(EngineError::Conflict(ConflictKind::RotationConflict(
                format!("kid {} already installed with a different secret", args.new_kid),
            )));
        }

        let prior_active: Option<(String,)> = sqlx::query_as(q_wp::SELECT_ACTIVE_KID_SQL)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let _ = args.grace_ms;

        sqlx::query(q_wp::DEACTIVATE_ALL_HMAC_SQL)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(q_wp::INSERT_HMAC_ROW_SQL)
            .bind(&args.new_kid)
            .bind(&secret_bytes)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        Ok(RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid: prior_active.map(|(k,)| k),
            new_kid: args.new_kid.clone(),
            gc_count: 0,
        })
    }
    .await;

    match &outcome_res {
        Ok(_) => commit_or_rollback(&mut conn).await?,
        Err(_) => rollback_quiet(&mut conn).await,
    }

    Ok(RotateWaitpointHmacSecretAllResult::new(vec![
        RotateWaitpointHmacSecretAllEntry::new(0, outcome_res),
    ]))
}
