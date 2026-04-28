//! Admission-pipeline for the Postgres backend (RFC-v0.7 Wave 5b).
//!
//! Mirrors `ff-scheduler::claim::Scheduler::claim_for_worker` on Valkey:
//! find an eligible execution, match capabilities, admit under budget,
//! admit under quota, and issue a signed [`ClaimGrant`]. Returns
//! `Ok(None)` when no candidate is admissible.
//!
//! # Pipeline
//!
//! All six pipeline stages run in one `READ COMMITTED` transaction
//! (Q11):
//!
//! 1. **Eligible pick.** `SELECT ... FROM ff_exec_core WHERE lane_id =
//!    $1 AND lifecycle_phase = 'runnable' AND eligibility_state =
//!    'eligible_now' ORDER BY priority DESC, created_at_ms ASC FOR
//!    UPDATE SKIP LOCKED LIMIT N`. Over-fetches so capability-subset
//!    filtering has candidates after per-row rejects.
//! 2. **Capability subset-match.** Each over-fetched row is filtered
//!    via [`ff_core::caps::matches`]. First matching row wins.
//! 3. **Budget admission.** If the exec row carries `budget_ids`,
//!    each referenced [`ff_budget_policy`] row is `FOR SHARE`-locked,
//!    its `policy_json` parsed for `hard_limit` + `dimension`, and
//!    the current `ff_budget_usage` value compared. Breach →
//!    `Ok(None)`, row left eligible for another worker/tick.
//! 4. **Quota admission.** There is no quota schema in 0001/0002.
//!    This stage is a no-op and reported as "skipped — quota schema
//!    not yet migrated" in the return channel. Grep of the migrations
//!    directory confirms: only budget + core + suspension + stream
//!    tables exist.
//! 5. **Issue ClaimGrant.** Uses the Wave-4d global
//!    [`ff_waitpoint_hmac`] keystore via [`hmac_sign`] — the same
//!    primitive signing waitpoint tokens. The signed blob rides
//!    inside [`ClaimGrant::grant_key`] as
//!    `pg:<hash-tag>:<uuid>:<expires_ms>:<kid>:<hex>`. The grant
//!    itself is stashed into `ff_exec_core.raw_fields.claim_grant`
//!    (no schema addition — Wave-4b already uses `raw_fields` as the
//!    untyped-column overflow; see `progress`).
//! 6. **Commit + return grant.**
//!
//! # Isolation (Q11)
//!
//! READ COMMITTED + `FOR UPDATE SKIP LOCKED` on the eligible pick +
//! `FOR SHARE` on budget policy rows. No SERIALIZABLE retries.
//!
//! # Scheduler integration
//!
//! `ff-scheduler` today is Valkey-specific (`ferriskey::Client`
//! embedded in `Scheduler`). Rather than add `ff-backend-postgres`
//! (and its sqlx transitive graph) as a dep of ff-scheduler — which
//! would pollute every consumer (ff-server, ff-observability,
//! ff-test, ff-readiness-tests, ff-script, ff-backend-valkey) — the
//! Postgres variant lives here as a free-standing [`PostgresScheduler`]
//! struct. ff-server dispatches against the concrete backend type it
//! constructed (Valkey → `ff_scheduler::Scheduler`, Postgres →
//! `ff_backend_postgres::scheduler::PostgresScheduler`); no trait-
//! object indirection needed because the engine already decides
//! backend at boot.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use ff_core::caps::{matches as caps_matches, CapabilityRequirement};
use ff_core::contracts::ClaimGrant;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{ExecutionId, LaneId, WorkerId, WorkerInstanceId};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::signal::{current_active_kid, hmac_sign};

/// Over-fetch size for the eligible pick. Gives per-row capability
/// filter some headroom before giving up — matches the Valkey path's
/// "pick 10 and filter" shape.
const ELIGIBLE_OVERFETCH: i64 = 10;

/// Postgres admission pipeline. See the module rustdoc for
/// pipeline + isolation notes.
pub struct PostgresScheduler {
    pool: PgPool,
}

impl PostgresScheduler {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Find an eligible execution, admit it against budget + quota,
    /// and issue a signed [`ClaimGrant`]. Returns `Ok(None)` when no
    /// candidate is admissible on this lane right now.
    pub async fn claim_for_worker(
        &self,
        lane: &LaneId,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        worker_capabilities: &BTreeSet<String>,
        grant_ttl_ms: u64,
    ) -> Result<Option<ClaimGrant>, EngineError> {
        // Read the active HMAC kid up-front — if the keystore is
        // empty we refuse to issue grants (fail-closed, matches the
        // Valkey path where ff_issue_claim_grant requires a secret).
        let (kid, secret) = match current_active_kid(&self.pool).await? {
            Some(v) => v,
            None => {
                return Err(EngineError::Unavailable {
                    op: "claim_for_worker: ff_waitpoint_hmac keystore empty",
                });
            }
        };

        // Iterate partitions 0..256. Over each partition we run the
        // full admission tx. Matches the Valkey scheduler's partition-
        // walk shape; the bounded-scan / rotation-cursor machinery is
        // Valkey-specific (lives in ff-scheduler) and not ported here.
        const TOTAL_PARTITIONS: i16 = 256;
        for part in 0..TOTAL_PARTITIONS {
            if let Some(grant) = self
                .try_claim_in_partition(
                    part,
                    lane,
                    worker_id,
                    worker_instance_id,
                    worker_capabilities,
                    grant_ttl_ms,
                    &kid,
                    &secret,
                )
                .await?
            {
                return Ok(Some(grant));
            }
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_claim_in_partition(
        &self,
        part: i16,
        lane: &LaneId,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        worker_capabilities: &BTreeSet<String>,
        grant_ttl_ms: u64,
        kid: &str,
        secret: &[u8],
    ) -> Result<Option<ClaimGrant>, EngineError> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;

        // ── Stage 1: eligible pick (FOR UPDATE SKIP LOCKED) ──
        let rows = sqlx::query(
            r#"
            SELECT execution_id, required_capabilities, raw_fields
              FROM ff_exec_core
             WHERE partition_key = $1
               AND lane_id = $2
               AND lifecycle_phase = 'runnable'
               AND eligibility_state = 'eligible_now'
             ORDER BY priority DESC, created_at_ms ASC
             FOR UPDATE SKIP LOCKED
             LIMIT $3
            "#,
        )
        .bind(part)
        .bind(lane.as_str())
        .bind(ELIGIBLE_OVERFETCH)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        if rows.is_empty() {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(None);
        }

        // ── Stage 2: capability subset-match ──
        let mut picked: Option<(Uuid, JsonValue)> = None;
        for row in &rows {
            let required: Vec<String> = row
                .try_get::<Vec<String>, _>("required_capabilities")
                .map_err(map_sqlx_error)?;
            let req = CapabilityRequirement::new(required);
            let worker_set = ff_core::backend::CapabilitySet::new(worker_capabilities.iter().cloned());
            if !caps_matches(&req, &worker_set) {
                continue;
            }
            let eid: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
            let raw: JsonValue = row.try_get("raw_fields").map_err(map_sqlx_error)?;
            picked = Some((eid, raw));
            break;
        }
        let Some((exec_uuid, raw_fields)) = picked else {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(None);
        };

        // ── Stage 3: budget admission ──
        // `budget_ids` is stashed in raw_fields as a comma-separated
        // string (matches the Valkey HGET shape). Missing field → no
        // budget attached; empty string → no budget attached.
        let budget_ids: Vec<String> = raw_fields
            .get("budget_ids")
            .and_then(JsonValue::as_str)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        for bid in &budget_ids {
            if !admit_budget(&mut tx, bid).await? {
                // Breach — leave the row eligible (rollback releases
                // the FOR UPDATE lock without mutating the row).
                tx.rollback().await.map_err(map_sqlx_error)?;
                return Ok(None);
            }
        }

        // ── Stage 4: quota admission (no schema → skipped) ──
        // Quota tables are not in 0001_initial.sql or 0002_budget.sql.
        // A future migration (0003_quota.sql) would add
        // ff_quota_policy + ff_quota_usage; this stage becomes a
        // FOR SHARE policy-read + current-count compare at that
        // point. Keeping the slot in the pipeline as a doc/ack:
        let _quota_skipped_no_schema = ();

        // ── Stage 5: issue signed ClaimGrant ──
        let now = now_ms();
        let expires_at_ms = now.saturating_add_unsigned(grant_ttl_ms.min(i64::MAX as u64));

        // Sign over the fence-relevant fields. Verification later
        // re-constructs the same message.
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: part as u16,
        };
        let hash_tag = partition.hash_tag();
        let message = format!(
            "{hash_tag}|{exec_uuid}|{wid}|{wiid}|{exp}",
            wid = worker_id.as_str(),
            wiid = worker_instance_id.as_str(),
            exp = expires_at_ms,
        );
        let sig = hmac_sign(secret, kid, message.as_bytes());
        let grant_key = format!("pg:{hash_tag}:{exec_uuid}:{expires_at_ms}:{sig}");

        // RFC-024 PR-D: persist the grant into `ff_claim_grant` (the
        // properly-shaped table with `kind` discriminator). Pre-PR-D
        // this used the JSON stash at `ff_exec_core.raw_fields.claim_grant`
        // — the JSON column is left in place for one release for
        // backfill safety (RFC §5 / §10) but is no longer consulted
        // by the read path.
        crate::claim_grant::write_claim_grant(
            &mut tx,
            part,
            &grant_key,
            exec_uuid,
            worker_id.as_str(),
            worker_instance_id.as_str(),
            grant_ttl_ms,
            now,
            expires_at_ms,
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET eligibility_state = 'pending_claim'
             WHERE partition_key = $1 AND execution_id = $2
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        tx.commit().await.map_err(map_sqlx_error)?;

        // Build ClaimGrant wire-type.
        let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).map_err(|e| {
            EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("scheduler: reassembling exec id: {e}"),
            }
        })?;
        Ok(Some(ClaimGrant::new(
            eid,
            PartitionKey::from(&partition),
            grant_key,
            expires_at_ms as u64,
        )))
    }
}

/// Verify a grant produced by [`PostgresScheduler::claim_for_worker`].
///
/// Returns `Ok(())` iff the signature embedded in `grant.grant_key`
/// verifies under the kid stored with it and `expires_at_ms` is in
/// the future. Exposed for test / consumer use; a production
/// `claim_from_grant` on Postgres will extend this with fence-check
/// into `ff_attempt`.
pub async fn verify_grant(pool: &PgPool, grant: &ClaimGrant) -> Result<(), GrantVerifyError> {
    // Parse: pg:<hash-tag>:<uuid>:<expires_ms>:<kid>:<hex>
    let s = grant.grant_key.as_str();
    let rest = s.strip_prefix("pg:").ok_or(GrantVerifyError::Malformed)?;
    // hash_tag contains ':' internally ("{fp:7}"), so split from the
    // right: kid:hex is the final `kid:hex` segment, preceded by
    // expires_ms, preceded by uuid, preceded by the hash tag.
    let mut parts: Vec<&str> = rest.rsplitn(4, ':').collect(); // [hex, kid, expires_ms, hash_tag:uuid?]
    // rsplitn from the right yields in reverse order; we need 4
    // segments: hex, kid, expires, and the left-over prefix
    // hash_tag:uuid (which still contains one `:`).
    if parts.len() != 4 {
        return Err(GrantVerifyError::Malformed);
    }
    let hex_part = parts.remove(0);
    let kid = parts.remove(0);
    let expires_str = parts.remove(0);
    let left = parts.remove(0); // "{fp:N}:<uuid>"
    let expires_at_ms: i64 = expires_str.parse().map_err(|_| GrantVerifyError::Malformed)?;
    if expires_at_ms <= now_ms() {
        return Err(GrantVerifyError::Expired);
    }
    // Split left into hash_tag and uuid. hash_tag ends at `}`.
    let close = left.find("}:").ok_or(GrantVerifyError::Malformed)?;
    let hash_tag = &left[..=close]; // includes `}`
    let uuid_str = &left[close + 2..];

    // Look up the kid's secret (may be inactive — grace window).
    let secret = crate::signal::fetch_kid(pool, kid)
        .await
        .map_err(|_| GrantVerifyError::Transport)?
        .ok_or(GrantVerifyError::UnknownKid)?;

    // Reconstruct the signed message.
    let wid_wiid = read_grant_identity(pool, grant).await?;
    let message = format!(
        "{hash_tag}|{uuid_str}|{wid}|{wiid}|{expires_at_ms}",
        wid = wid_wiid.0,
        wiid = wid_wiid.1,
    );
    let token = format!("{kid}:{hex_part}");
    crate::signal::hmac_verify(&secret, kid, message.as_bytes(), &token)
        .map_err(|_| GrantVerifyError::SignatureMismatch)?;
    Ok(())
}

/// Read the worker identity for a claim grant by (partition_key,
/// execution_id). RFC-024 PR-D: reads from the new `ff_claim_grant`
/// table (kind='claim'); pre-PR-D this read from
/// `ff_exec_core.raw_fields.claim_grant`.
async fn read_grant_identity(
    pool: &PgPool,
    grant: &ClaimGrant,
) -> Result<(String, String), GrantVerifyError> {
    let partition = grant.partition().map_err(|_| GrantVerifyError::Malformed)?;
    let part = partition.index as i16;
    let uuid_str = grant
        .execution_id
        .as_str()
        .split_once("}:")
        .map(|(_, u)| u)
        .ok_or(GrantVerifyError::Malformed)?;
    let exec_uuid = Uuid::parse_str(uuid_str).map_err(|_| GrantVerifyError::Malformed)?;
    let ident = crate::claim_grant::read_claim_grant_identity(pool, part, exec_uuid)
        .await
        .map_err(|_| GrantVerifyError::Transport)?
        .ok_or(GrantVerifyError::UnknownGrant)?;
    Ok(ident)
}

/// Errors from [`verify_grant`].
#[derive(Debug, thiserror::Error)]
pub enum GrantVerifyError {
    #[error("grant_key malformed")]
    Malformed,
    #[error("grant expired")]
    Expired,
    #[error("unknown kid in grant")]
    UnknownKid,
    #[error("unknown grant — no row with matching claim_grant in exec_core")]
    UnknownGrant,
    #[error("signature verification failed")]
    SignatureMismatch,
    #[error("transport error while verifying grant")]
    Transport,
}

/// Budget admission for a single budget_id. Returns `Ok(true)` when
/// the budget admits (or is unknown — fail-open on missing policy,
/// matching the Valkey fallback on non-UUID test IDs), `Ok(false)`
/// on hard breach.
async fn admit_budget(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    budget_id: &str,
) -> Result<bool, EngineError> {
    // Compute partition for this budget. The id might be a UUID or a
    // test literal — fall back to partition 0 on parse failure, same
    // as the Valkey BudgetChecker.
    let partition_key: i16 = ff_core::types::BudgetId::parse(budget_id)
        .map(|bid| {
            ff_core::partition::budget_partition(&bid, &ff_core::partition::PartitionConfig::default())
                .index as i16
        })
        .unwrap_or(0);

    // FOR SHARE on the policy row — protects against concurrent
    // rotation while we read hard_limit.
    let policy: Option<JsonValue> = sqlx::query_scalar(
        r#"
        SELECT policy_json FROM ff_budget_policy
         WHERE partition_key = $1 AND budget_id = $2
         FOR SHARE
        "#,
    )
    .bind(partition_key)
    .bind(budget_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(policy) = policy else {
        // No policy row — nothing to enforce.
        return Ok(true);
    };

    // Extract hard_limit + dimension. Shape matches the Wave 4f
    // upsert_policy_for_test fixture: { "dimension": "tokens",
    // "hard_limit": <u64> } (either at top-level or under "hard").
    let hard_limit = policy
        .get("hard_limit")
        .and_then(JsonValue::as_u64)
        .or_else(|| {
            policy
                .get("hard")
                .and_then(JsonValue::as_object)
                .and_then(|o| o.values().next())
                .and_then(JsonValue::as_u64)
        });
    let dimension = policy
        .get("dimension")
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "default".to_owned());
    let Some(hard_limit) = hard_limit else {
        return Ok(true);
    };

    // Sum current_value across dimension rows. A missing usage row
    // means 0 used.
    let current: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT current_value FROM ff_budget_usage
         WHERE partition_key = $1 AND budget_id = $2 AND dimensions_key = $3
         FOR SHARE
        "#,
    )
    .bind(partition_key)
    .bind(budget_id)
    .bind(&dimension)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    let current = current.unwrap_or(0).max(0) as u64;

    // Hard-limit rule: must have room for at least one more unit. No
    // pre-reservation — the worker reports usage after execution.
    Ok(current < hard_limit)
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}
