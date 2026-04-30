//! #33 / Phase 1: integration test for SQLite
//! `EngineBackend::check_admission`.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{CheckAdmissionArgs, CheckAdmissionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{ExecutionId, QuotaPolicyId, TimestampMs};
use serial_test::serial;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now_ms() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn uuid_like() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:typed-admit-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("construct")
}

fn fresh_eid() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).unwrap()
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn admits_first_request_in_window() {
    let backend = fresh_backend().await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    let args = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 10,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    let res = be.check_admission(&qid, "", args).await.expect("admit");
    assert_eq!(res, CheckAdmissionResult::Admitted);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn idempotent_second_admit_returns_already_admitted() {
    let backend = fresh_backend().await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    let eid = fresh_eid();
    let args = CheckAdmissionArgs {
        execution_id: eid,
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 10,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    assert_eq!(
        be.check_admission(&qid, "", args.clone()).await.unwrap(),
        CheckAdmissionResult::Admitted
    );
    assert_eq!(
        be.check_admission(&qid, "", args).await.unwrap(),
        CheckAdmissionResult::AlreadyAdmitted
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn rate_exceeded_returns_retry_after() {
    let backend = fresh_backend().await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    for _ in 0..3 {
        let args = CheckAdmissionArgs {
            execution_id: fresh_eid(),
            now: TimestampMs::from_millis(now_ms()),
            window_seconds: 60,
            rate_limit: 3,
            concurrency_cap: 100,
            jitter_ms: None,
        };
        assert_eq!(
            be.check_admission(&qid, "", args).await.unwrap(),
            CheckAdmissionResult::Admitted
        );
    }
    let fourth = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 3,
        concurrency_cap: 100,
        jitter_ms: None,
    };
    match be.check_admission(&qid, "", fourth).await.unwrap() {
        CheckAdmissionResult::RateExceeded { .. } => {}
        other => panic!("expected RateExceeded, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn concurrency_cap_blocks_additional_admits() {
    let backend = fresh_backend().await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let qid = QuotaPolicyId::from_uuid(Uuid::new_v4());
    // Pre-seed policy row with active_concurrency = 2, cap = 2.
    let part = ff_core::partition::quota_partition(
        &qid,
        &ff_core::partition::PartitionConfig::default(),
    )
    .index as i64;
    sqlx::query(
        "INSERT INTO ff_quota_policy \
            (partition_key, quota_policy_id, \
             requests_per_window_seconds, max_requests_per_window, \
             active_concurrency_cap, active_concurrency, \
             created_at_ms, updated_at_ms) \
         VALUES (?1, ?2, 60, 100, 2, 2, ?3, ?3)",
    )
    .bind(part)
    .bind(qid.to_string())
    .bind(now_ms())
    .execute(backend.pool_for_test())
    .await
    .expect("seed policy");
    let args = CheckAdmissionArgs {
        execution_id: fresh_eid(),
        now: TimestampMs::from_millis(now_ms()),
        window_seconds: 60,
        rate_limit: 100,
        concurrency_cap: 2,
        jitter_ms: None,
    };
    match be.check_admission(&qid, "", args).await.unwrap() {
        CheckAdmissionResult::ConcurrencyExceeded => {}
        other => panic!("expected ConcurrencyExceeded, got {other:?}"),
    }
}
