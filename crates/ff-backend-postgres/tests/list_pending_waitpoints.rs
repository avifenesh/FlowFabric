//! RFC-020 Wave 9 Standalone-2 — integration tests for
//! `list_pending_waitpoints` + producer-side writes of the 0011
//! columns (`state`, `required_signal_names`, `activated_at_ms`).
//!
//! Follows the crate-local `FF_PG_TEST_URL` / `#[ignore]` convention
//! used by `suspend_signal.rs` (per-test schema shadow of the global
//! `ff_waitpoint_hmac` keystore — see issue #301).
//!
//! Covers:
//!
//! * [`producer_writes_active_state_and_activated_at`] — insert
//!   populates `state = 'active'` + `activated_at_ms = now` (no
//!   separate `pending→active` transition exists on Postgres; the
//!   suspension lands atomically).
//! * [`producer_writes_required_signal_names_byname`] — `Single`
//!   with `SignalMatcher::ByName` produces a `[name]` vec.
//! * [`producer_writes_required_signal_names_wildcard`] — `Single`
//!   with `SignalMatcher::Wildcard` produces an empty vec (wildcard
//!   wire marker).
//! * [`producer_writes_required_signal_names_allof`] — `AllOf` tree
//!   distributes per-waitpoint names correctly.
//! * [`list_returns_not_found_for_missing_execution`] — matches
//!   Valkey's `EXISTS exec_core` pre-check.
//! * [`list_returns_single_waitpoint`] — happy-path end-to-end:
//!   suspend, then list, verify all 10 contract fields.
//! * [`list_paginates_with_cursor`] — multi-waitpoint AllOf, page
//!   size 1, cursor threads correctly.

use ff_backend_postgres::signal::rotate_waitpoint_hmac_secret_all_impl;
use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{BackendTag, Handle, HandleKind};
use ff_core::contracts::{
    CompositeBody, ListPendingWaitpointsArgs, ResumeCondition, ResumePolicy,
    RotateWaitpointHmacSecretAllArgs, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::handle_codec::{encode as encode_opaque, HandlePayload};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, SuspensionId, TimestampMs,
    WaitpointId, WorkerInstanceId,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

// ─── per-test pool + schema (shadow `ff_waitpoint_hmac`) ─────────────────

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;

    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&bootstrap)
        .await
        .expect("apply_migrations clean");

    let schema = format!("ffpg_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&bootstrap)
        .await
        .expect("create per-test schema");
    sqlx::query(&format!(
        "CREATE TABLE {schema}.ff_waitpoint_hmac \
         (LIKE public.ff_waitpoint_hmac INCLUDING ALL)"
    ))
    .execute(&bootstrap)
    .await
    .expect("create per-test ff_waitpoint_hmac");
    bootstrap.close().await;

    let schema_for_hook = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _meta| {
            let schema = schema_for_hook.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {schema}, public"))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");

    Some(pool)
}

struct ExecFixture {
    backend: std::sync::Arc<dyn EngineBackend>,
    pool: PgPool,
    exec_id: ExecutionId,
    part: i16,
    exec_uuid: Uuid,
    handle: Handle,
}

async fn setup_exec_or_skip() -> Option<ExecFixture> {
    let pool = setup_or_skip().await?;

    rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        RotateWaitpointHmacSecretAllArgs::new("kid-lpw-1", "cd".repeat(32), 0),
        1_000_000,
    )
    .await
    .unwrap();

    let lane = LaneId::new("default");
    let exec_id = ExecutionId::solo(&lane, &PartitionConfig::default());
    let part = exec_id.partition() as i16;
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();

    let now = TimestampMs::now().0;
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, created_at_ms) \
         VALUES ($1, $2, $3, 0, 'active', 'leased', 'not_applicable', \
                 'running', 'running_attempt', $4)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane.as_str())
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert exec_core");

    let attempt_index = AttemptIndex::new(0);
    let lease_epoch = LeaseEpoch(1);
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, worker_id, \
            worker_instance_id, lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES ($1, $2, 0, 'w1', 'wi1', 1, $3, $4)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now + 30_000)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert attempt");

    let payload = HandlePayload::new(
        exec_id.clone(),
        attempt_index,
        AttemptId::new(),
        LeaseId::new(),
        lease_epoch,
        30_000,
        lane.clone(),
        WorkerInstanceId::new("wi1"),
    );
    let opaque = encode_opaque(BackendTag::Postgres, &payload);
    let handle = Handle::new(BackendTag::Postgres, HandleKind::Fresh, opaque);

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default())
        as std::sync::Arc<dyn EngineBackend>;

    Some(ExecFixture {
        backend,
        pool,
        exec_id,
        part,
        exec_uuid,
        handle,
    })
}

// ─── producer-side column-write tests ────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn producer_writes_active_state_and_activated_at() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };

    let wp_id = WaitpointId::new();
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_id.clone(),
            waitpoint_key: "wpk:active-state".to_owned(),
        },
        ResumeCondition::Single {
            waitpoint_key: "wpk:active-state".to_owned(),
            matcher: SignalMatcher::ByName("ready".into()),
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let before_ms = args.now.0;
    fx.backend.suspend(&fx.handle, args).await.expect("suspend");

    let row: (String, Option<i64>, Vec<String>) = sqlx::query_as(
        "SELECT state, activated_at_ms, required_signal_names \
           FROM ff_waitpoint_pending \
          WHERE partition_key = $1 AND waitpoint_id = $2",
    )
    .bind(fx.part)
    .bind(Uuid::parse_str(&wp_id.to_string()).unwrap())
    .fetch_one(&fx.pool)
    .await
    .expect("fetch waitpoint row");

    assert_eq!(row.0, "active", "Postgres suspend lands waitpoint active");
    assert!(
        row.1.is_some() && row.1.unwrap() >= before_ms,
        "activated_at_ms populated on insert"
    );
    assert_eq!(row.2, vec!["ready".to_owned()]);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn producer_writes_required_signal_names_wildcard() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };

    let wp_id = WaitpointId::new();
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_id.clone(),
            waitpoint_key: "wpk:wild".to_owned(),
        },
        ResumeCondition::Single {
            waitpoint_key: "wpk:wild".to_owned(),
            matcher: SignalMatcher::Wildcard,
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    fx.backend.suspend(&fx.handle, args).await.expect("suspend");

    let row: (Vec<String>,) = sqlx::query_as(
        "SELECT required_signal_names FROM ff_waitpoint_pending \
          WHERE partition_key = $1 AND waitpoint_id = $2",
    )
    .bind(fx.part)
    .bind(Uuid::parse_str(&wp_id.to_string()).unwrap())
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(
        row.0.is_empty(),
        "wildcard matcher → empty required_signal_names (wire marker)"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn producer_writes_required_signal_names_allof() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };

    let wp_a_id = WaitpointId::new();
    let wp_b_id = WaitpointId::new();
    let cond = ResumeCondition::Composite(CompositeBody::AllOf {
        members: vec![
            ResumeCondition::Single {
                waitpoint_key: "wpk:a".into(),
                matcher: SignalMatcher::ByName("sig-a".into()),
            },
            ResumeCondition::Single {
                waitpoint_key: "wpk:b".into(),
                matcher: SignalMatcher::ByName("sig-b".into()),
            },
        ],
    });
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_a_id.clone(),
            waitpoint_key: "wpk:a".into(),
        },
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp_b_id.clone(),
        waitpoint_key: "wpk:b".into(),
    });
    fx.backend.suspend(&fx.handle, args).await.expect("suspend");

    let a: (Vec<String>,) = sqlx::query_as(
        "SELECT required_signal_names FROM ff_waitpoint_pending \
          WHERE partition_key = $1 AND waitpoint_id = $2",
    )
    .bind(fx.part)
    .bind(Uuid::parse_str(&wp_a_id.to_string()).unwrap())
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(a.0, vec!["sig-a".to_owned()]);

    let b: (Vec<String>,) = sqlx::query_as(
        "SELECT required_signal_names FROM ff_waitpoint_pending \
          WHERE partition_key = $1 AND waitpoint_id = $2",
    )
    .bind(fx.part)
    .bind(Uuid::parse_str(&wp_b_id.to_string()).unwrap())
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(b.0, vec!["sig-b".to_owned()]);
}

// ─── list_pending_waitpoints method tests ────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_returns_not_found_for_missing_execution() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default())
        as std::sync::Arc<dyn EngineBackend>;

    let missing = ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default());
    let err = backend
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(missing))
        .await
        .expect_err("missing exec → NotFound");
    match err {
        EngineError::NotFound { entity } => assert_eq!(entity, "execution"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_returns_single_waitpoint() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };

    let wp_id = WaitpointId::new();
    let now = TimestampMs::now();
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp_id.clone(),
            waitpoint_key: "wpk:lookup".into(),
        },
        ResumeCondition::Single {
            waitpoint_key: "wpk:lookup".into(),
            matcher: SignalMatcher::ByName("ready".into()),
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        now,
    );
    let outcome = fx.backend.suspend(&fx.handle, args).await.expect("suspend");
    let SuspendOutcome::Suspended { .. } = outcome else {
        panic!("expected Suspended");
    };

    let page = fx
        .backend
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(fx.exec_id.clone()))
        .await
        .expect("list ok");

    assert_eq!(page.entries.len(), 1);
    assert!(page.next_cursor.is_none(), "single entry → no next_cursor");
    let entry = &page.entries[0];
    assert_eq!(entry.waitpoint_id, wp_id);
    assert_eq!(entry.waitpoint_key, "wpk:lookup");
    assert_eq!(entry.state, "active");
    assert_eq!(entry.required_signal_names, vec!["ready".to_owned()]);
    assert_eq!(entry.created_at.0, now.0);
    assert_eq!(
        entry.activated_at.map(|t| t.0),
        Some(now.0),
        "activated_at populated"
    );
    assert!(entry.expires_at.is_none());
    assert_eq!(entry.execution_id, fx.exec_id);
    assert_eq!(entry.token_kid, "kid-lpw-1");
    // Fingerprint is first-16-hex of the HMAC body; validate shape only.
    assert_eq!(
        entry.token_fingerprint.len(),
        16,
        "token_fingerprint is 16 hex chars"
    );
    assert!(
        entry
            .token_fingerprint
            .chars()
            .all(|c| c.is_ascii_hexdigit()),
        "fingerprint is hex"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_paginates_with_cursor() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };

    // Suspend with 3 waitpoints via AllOf.
    let wp1 = WaitpointId::new();
    let wp2 = WaitpointId::new();
    let wp3 = WaitpointId::new();
    let cond = ResumeCondition::Composite(CompositeBody::AllOf {
        members: vec![
            ResumeCondition::Single {
                waitpoint_key: "wpk:1".into(),
                matcher: SignalMatcher::ByName("s1".into()),
            },
            ResumeCondition::Single {
                waitpoint_key: "wpk:2".into(),
                matcher: SignalMatcher::ByName("s2".into()),
            },
            ResumeCondition::Single {
                waitpoint_key: "wpk:3".into(),
                matcher: SignalMatcher::ByName("s3".into()),
            },
        ],
    });
    let args = SuspendArgs::new(
        SuspensionId::new(),
        WaitpointBinding::Fresh {
            waitpoint_id: wp1.clone(),
            waitpoint_key: "wpk:1".into(),
        },
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    )
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp2.clone(),
        waitpoint_key: "wpk:2".into(),
    })
    .with_waitpoint(WaitpointBinding::Fresh {
        waitpoint_id: wp3.clone(),
        waitpoint_key: "wpk:3".into(),
    });
    fx.backend.suspend(&fx.handle, args).await.expect("suspend");

    // Page 1: limit=2 → 2 entries + next_cursor.
    let p1 = fx
        .backend
        .list_pending_waitpoints(
            ListPendingWaitpointsArgs::new(fx.exec_id.clone()).with_limit(2),
        )
        .await
        .expect("list p1");
    assert_eq!(p1.entries.len(), 2);
    let cursor = p1.next_cursor.clone().expect("next_cursor present");

    // Page 2: resume from cursor → remaining entry, no further cursor.
    let p2 = fx
        .backend
        .list_pending_waitpoints(
            ListPendingWaitpointsArgs::new(fx.exec_id.clone())
                .with_after(cursor)
                .with_limit(2),
        )
        .await
        .expect("list p2");
    assert_eq!(p2.entries.len(), 1);
    assert!(p2.next_cursor.is_none());

    // Union of pages covers all 3 waitpoints (ordering is by
    // waitpoint_id uuid, not insertion order).
    let mut all: Vec<WaitpointId> =
        p1.entries.iter().chain(p2.entries.iter()).map(|e| e.waitpoint_id.clone()).collect();
    all.sort_by_key(|w| w.to_string());
    let mut expected = vec![wp1, wp2, wp3];
    expected.sort_by_key(|w| w.to_string());
    assert_eq!(all, expected);

    let _ = fx.exec_uuid; // suppress unused-field warning
}
