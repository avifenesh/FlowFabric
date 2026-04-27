//! RFC-023 Phase 3.1 — subscribe_completion / subscribe_lease_history /
//! subscribe_signal_delivery integration tests.
//!
//! Covers for each method:
//!   * tail + live-wake happy path (subscriber observes an event
//!     produced after subscribe returns)
//!   * cursor-resume (resubscribe with the last observed cursor,
//!     skip already-seen events)
//!   * `ScannerFilter::instance_tag` filtering (where the producer
//!     populates the denormalised column; lease/completion producers
//!     write NULL today, so only signal_delivery has an end-to-end
//!     filter test — documented in the test comment)
//!   * lagged-broadcast recovery (force the broadcast ring to
//!     overflow via many back-pressured emits; the cursor-select
//!     fallback catches the drops)

#![cfg(feature = "core")]

use std::sync::Arc;
use std::time::Duration;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy, ScannerFilter};
use ff_core::contracts::{
    CreateExecutionArgs, CreateExecutionResult, DeliverSignalArgs, DeliverSignalResult,
    ResumeCondition, ResumePolicy, SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::stream_events::{
    CompletionEvent, CompletionOutcome, LeaseHistoryEvent, SignalDeliveryEvent,
};
use ff_core::stream_subscribe::StreamCursor;
use ff_core::types::{
    ExecutionId, LaneId, Namespace, SignalId, SuspensionId, TimestampMs, WaitpointId, WorkerId,
    WorkerInstanceId,
};
use futures_core::Stream;
use serial_test::serial;
use std::future::poll_fn;
use std::pin::Pin;
use uuid::Uuid;

// ── Setup helpers ─────────────────────────────────────────────────────

const KID: &str = "kid-test";

fn secret_hex() -> String {
    "cafebabe".repeat(8)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    // SAFETY: test-only env mutation; every caller is serial-gated.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:rfc-023-subscribe-{}?mode=memory&cache=shared", uuid_like());
    let b = SqliteBackend::new(&uri).await.expect("construct backend");
    b.seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed kid");
    b
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

/// Build a `CreateExecutionArgs` with optional `(tag_key, tag_value)`
/// so filter tests can pass `cairn.instance_id = ...` and observe
/// the denormalised column on `ff_signal_event`.
fn create_args(exec_id: &ExecutionId, tag: Option<(&str, &str)>) -> CreateExecutionArgs {
    let mut tags = std::collections::HashMap::new();
    if let Some((k, v)) = tag {
        tags.insert(k.to_owned(), v.to_owned());
    }
    CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: LaneId::new("default"),
        execution_kind: "op".into(),
        input_payload: b"hello".to_vec(),
        payload_encoding: None,
        priority: 0,
        creator_identity: "test".into(),
        idempotency_key: None,
        tags,
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(now_ms()),
    }
}

async fn create_and_claim_tagged(
    backend: &Arc<SqliteBackend>,
    tag: Option<(&str, &str)>,
) -> (ExecutionId, ff_core::backend::Handle) {
    // Each call uses a unique lane so that re-eligible executions from
    // earlier calls (e.g. a signalled-but-not-completed resumable exec)
    // cannot be picked up by this claim. Without this, `claim()` would
    // happily return a stale handle bound to a PRIOR exec (same lane
    // pool) whose fence triple no longer matches the fresh exec id we
    // just created.
    let lane_id = LaneId::new(format!("lane-{}", Uuid::new_v4()));
    let exec_id = new_exec_id();
    let mut args = create_args(&exec_id, tag);
    args.lane_id = lane_id.clone();
    let r = backend.create_execution(args).await.expect("create");
    assert!(matches!(r, CreateExecutionResult::Created { .. }));
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', public_state='pending', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .unwrap();
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), policy)
        .await
        .expect("claim")
        .expect("handle");
    (exec_id, handle)
}

async fn create_and_claim(
    backend: &Arc<SqliteBackend>,
) -> (ExecutionId, ff_core::backend::Handle) {
    create_and_claim_tagged(backend, None).await
}

/// Collect up to `n` items from a boxed stream with an overall timeout.
async fn drain_n<T>(
    stream: &mut Pin<Box<dyn Stream<Item = Result<T, ff_core::engine_error::EngineError>> + Send>>,
    n: usize,
    timeout: Duration,
) -> Vec<T>
where
    T: 'static,
{
    let mut out = Vec::with_capacity(n);
    let deadline = tokio::time::Instant::now() + timeout;
    while out.len() < n {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        let poll_once = poll_fn(|cx| stream.as_mut().poll_next(cx));
        match tokio::time::timeout(remaining, poll_once).await {
            Ok(Some(Ok(ev))) => out.push(ev),
            Ok(Some(Err(e))) => panic!("stream yielded error: {e:?}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

fn suspend_args_wildcard(wp_key: &str) -> SuspendArgs {
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: WaitpointId::new(),
        waitpoint_key: wp_key.to_owned(),
    };
    SuspendArgs::new(
        SuspensionId::new(),
        binding,
        ResumeCondition::Single {
            waitpoint_key: wp_key.to_owned(),
            matcher: SignalMatcher::Wildcard,
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::from_millis(now_ms()),
    )
}

fn deliver_args(
    exec_id: &ExecutionId,
    waitpoint_id: WaitpointId,
    waitpoint_token: &str,
) -> DeliverSignalArgs {
    DeliverSignalArgs {
        execution_id: exec_id.clone(),
        waitpoint_id,
        signal_id: SignalId::new(),
        signal_name: "ready".into(),
        signal_category: "external".into(),
        source_type: "operator".into(),
        source_identity: "op-1".into(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: None,
        target_scope: "execution".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: ff_core::types::WaitpointToken::from(waitpoint_token.to_owned()),
        now: TimestampMs::from_millis(now_ms()),
    }
}

// ── subscribe_completion ──────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_completion_tail_happy_path() {
    let b = fresh_backend().await;
    // Subscribe FIRST so the catch-up SELECT starts from an empty
    // max-event_id; then produce one completion via `complete()`.
    let mut stream = b
        .subscribe_completion(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe");

    // Give the background task a moment to resolve the tail cursor.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (_eid, handle) = create_and_claim(&b).await;
    b.complete(&handle, None).await.expect("complete");

    let events = drain_n::<CompletionEvent>(&mut stream, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1, "expected one completion");
    assert!(matches!(events[0].outcome, CompletionOutcome::Success));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_completion_cursor_resume() {
    let b = fresh_backend().await;

    // Produce completion #1 BEFORE any subscriber — catch-up path.
    let (_e1, h1) = create_and_claim(&b).await;
    b.complete(&h1, None).await.expect("complete 1");

    // Subscribe from empty cursor; since completion #1 is already past
    // max, the tail subscriber sees NOTHING of event #1 (empty cursor
    // == tail-from-now). Complete #2 after subscribe.
    let mut stream = b
        .subscribe_completion(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe 1");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (_e2, h2) = create_and_claim(&b).await;
    b.complete(&h2, None).await.expect("complete 2");

    let events = drain_n::<CompletionEvent>(&mut stream, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1, "tail subscriber should only see #2");
    let cursor_after_2 = events[0].cursor.clone();
    drop(stream); // shut down the subscriber task

    // Reconnect with the cursor after completion #2; produce #3.
    let mut stream2 = b
        .subscribe_completion(cursor_after_2, &ScannerFilter::NOOP)
        .await
        .expect("subscribe 2");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (_e3, h3) = create_and_claim(&b).await;
    b.complete(&h3, None).await.expect("complete 3");

    let events = drain_n::<CompletionEvent>(&mut stream2, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1, "resume should yield #3 only");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_completion_filter_by_instance_tag_receives_events() {
    // Phase 3.2 fix: the SQLite completion producer now populates
    // `instance_tag` from `ff_exec_core.raw_fields.tags."cairn.instance_id"`
    // via a co-transactional SELECT (see
    // `queries/attempt.rs::INSERT_COMPLETION_EVENT_SQL`). A subscriber
    // filtering on a matching tag MUST observe the event; a subscriber
    // with a non-matching filter MUST drop it.
    let b = fresh_backend().await;
    let filter = ScannerFilter::new().with_instance_tag("cairn.instance_id", "i-42");
    let mut stream = b
        .subscribe_completion(StreamCursor::empty(), &filter)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Untagged completion — filter drops it.
    let (_e, h) = create_and_claim(&b).await;
    b.complete(&h, None).await.expect("complete untagged");

    // Tagged completion — filter passes it through.
    let (_e, h) = create_and_claim_tagged(&b, Some(("cairn.instance_id", "i-42"))).await;
    b.complete(&h, None).await.expect("complete tagged");

    let events =
        drain_n::<CompletionEvent>(&mut stream, 1, Duration::from_millis(2000)).await;
    assert_eq!(
        events.len(),
        1,
        "filter should pass through exactly one tagged completion event"
    );
}

// ── subscribe_lease_history ───────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_lease_history_tail_happy_path() {
    let b = fresh_backend().await;
    let mut stream = b
        .subscribe_lease_history(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (_eid, handle) = create_and_claim(&b).await;
    b.renew(&handle).await.expect("renew");
    b.complete(&handle, None).await.expect("complete");

    // create_and_claim emits "acquired"; renew emits "renewed";
    // complete emits "revoked". Three lease-history rows total.
    let events = drain_n::<LeaseHistoryEvent>(&mut stream, 3, Duration::from_millis(2000)).await;
    assert_eq!(events.len(), 3, "expected acquired+renewed+revoked, got {events:?}");
    assert!(matches!(events[0], LeaseHistoryEvent::Acquired { .. }));
    assert!(matches!(events[1], LeaseHistoryEvent::Renewed { .. }));
    assert!(matches!(events[2], LeaseHistoryEvent::Revoked { .. }));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_lease_history_cursor_resume() {
    let b = fresh_backend().await;
    let mut stream = b
        .subscribe_lease_history(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe 1");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_eid, handle) = create_and_claim(&b).await;
    let events =
        drain_n::<LeaseHistoryEvent>(&mut stream, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1);
    let cursor_after_1 = events[0].cursor().clone();
    drop(stream);

    // Renew on the same handle — produces "renewed". Resume from
    // `cursor_after_1` must yield "renewed" only, skipping the
    // already-seen "acquired".
    let mut stream2 = b
        .subscribe_lease_history(cursor_after_1, &ScannerFilter::NOOP)
        .await
        .expect("subscribe 2");
    tokio::time::sleep(Duration::from_millis(20)).await;
    b.renew(&handle).await.expect("renew");
    let events =
        drain_n::<LeaseHistoryEvent>(&mut stream2, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], LeaseHistoryEvent::Renewed { .. }));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_lease_history_filter_by_instance_tag_receives_events() {
    // Phase 3.2 fix: the SQLite lease_event producer now populates
    // `instance_tag` from `ff_exec_core.raw_fields.tags."cairn.instance_id"`
    // via a co-transactional SELECT (see
    // `queries/dispatch.rs::INSERT_LEASE_EVENT_SQL`). A subscriber
    // filtering on a matching tag MUST observe the full lease
    // lifecycle (acquired + revoked); a non-matching filter MUST drop.
    let b = fresh_backend().await;
    let filter = ScannerFilter::new().with_instance_tag("cairn.instance_id", "i-42");
    let mut stream = b
        .subscribe_lease_history(StreamCursor::empty(), &filter)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Untagged — filter drops every event.
    let (_eid, h_untagged) = create_and_claim(&b).await;
    b.complete(&h_untagged, None).await.expect("complete untagged");

    // Tagged — acquired + revoked both land with instance_tag=i-42,
    // both pass the filter.
    let (_eid, h_tagged) =
        create_and_claim_tagged(&b, Some(("cairn.instance_id", "i-42"))).await;
    b.complete(&h_tagged, None).await.expect("complete tagged");

    let events =
        drain_n::<LeaseHistoryEvent>(&mut stream, 2, Duration::from_millis(2000)).await;
    assert_eq!(
        events.len(),
        2,
        "tagged acquired + revoked should both pass the filter, got {events:?}"
    );
    assert!(matches!(events[0], LeaseHistoryEvent::Acquired { .. }));
    assert!(matches!(events[1], LeaseHistoryEvent::Revoked { .. }));
}

// ── subscribe_signal_delivery ─────────────────────────────────────────

async fn suspend_and_deliver(
    b: &Arc<SqliteBackend>,
    tag: Option<(&str, &str)>,
) -> ExecutionId {
    let (exec_id, handle) = create_and_claim_tagged(b, tag).await;
    let outcome = b
        .suspend(&handle, suspend_args_wildcard("wpk:sd"))
        .await
        .expect("suspend");
    let details = outcome.details().clone();
    let r = b
        .deliver_signal(deliver_args(
            &exec_id,
            details.waitpoint_id.clone(),
            details.waitpoint_token.as_str(),
        ))
        .await
        .expect("deliver");
    assert!(matches!(r, DeliverSignalResult::Accepted { .. }));
    exec_id
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_signal_delivery_tail_happy_path() {
    let b = fresh_backend().await;
    let mut stream = b
        .subscribe_signal_delivery(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    suspend_and_deliver(&b, None).await;

    let events =
        drain_n::<SignalDeliveryEvent>(&mut stream, 1, Duration::from_millis(2000)).await;
    assert_eq!(events.len(), 1, "expected one signal-delivery event");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_signal_delivery_cursor_resume() {
    let b = fresh_backend().await;
    let mut stream = b
        .subscribe_signal_delivery(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe 1");
    tokio::time::sleep(Duration::from_millis(20)).await;
    suspend_and_deliver(&b, None).await;
    let events =
        drain_n::<SignalDeliveryEvent>(&mut stream, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1);
    let cursor_after_1 = events[0].cursor.clone();
    drop(stream);

    // Resume: produce a second signal; observer must see only #2.
    let mut stream2 = b
        .subscribe_signal_delivery(cursor_after_1, &ScannerFilter::NOOP)
        .await
        .expect("subscribe 2");
    tokio::time::sleep(Duration::from_millis(20)).await;
    suspend_and_deliver(&b, None).await;
    let events =
        drain_n::<SignalDeliveryEvent>(&mut stream2, 1, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1, "resume should yield only the second signal");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_signal_delivery_filter_by_instance_tag() {
    // Signal producer DOES populate `instance_tag` from the exec's
    // `tags["cairn.instance_id"]` via json_extract (see
    // `queries/signal.rs::INSERT_SIGNAL_EVENT_SQL`), so an end-to-end
    // positive filter is observable here even though lease/completion
    // rows currently land with NULL filter columns.
    let b = fresh_backend().await;
    let filter = ScannerFilter::new().with_instance_tag("cairn.instance_id", "i-42");
    let mut stream = b
        .subscribe_signal_delivery(StreamCursor::empty(), &filter)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Produce a non-matching signal (no tag) — filter must drop it.
    suspend_and_deliver(&b, None).await;
    // Then a matching signal — filter passes it through.
    suspend_and_deliver(&b, Some(("cairn.instance_id", "i-42"))).await;

    let events =
        drain_n::<SignalDeliveryEvent>(&mut stream, 1, Duration::from_millis(2000)).await;
    assert_eq!(events.len(), 1, "only the tagged signal should pass the filter");
}

// ── lagged-broadcast recovery (shared) ────────────────────────────────
//
// Overwhelm the broadcast channel's 256-slot ring by producing a batch
// of events with the subscriber task stalled; when it polls,
// `RecvError::Lagged` fires and the cursor-select fallback catches
// every missed row via the durable outbox. `subscribe_completion` is
// exercised because the producer path is the simplest end-to-end:
// `create_and_claim` + `complete` in a tight loop — no suspend /
// waitpoint setup needed per event — so saturating the 256-slot
// broadcast ring takes a minimal number of iterations.

#[tokio::test]
#[serial(ff_dev_mode)]
async fn subscribe_completion_lagged_recovery() {
    let b = fresh_backend().await;
    let mut stream = b
        .subscribe_completion(StreamCursor::empty(), &ScannerFilter::NOOP)
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Produce more completions than the broadcast ring can hold
    // (DEFAULT_CAPACITY = 256). We sprint 300 completions; the
    // subscriber will observe a broadcast `Lagged` error the first
    // time it wakes, but the outbox-cursor fallback replays every
    // durable row from the last cursor.
    const N: usize = 300;
    for _ in 0..N {
        let (_e, h) = create_and_claim(&b).await;
        b.complete(&h, None).await.expect("complete");
    }

    let events =
        drain_n::<CompletionEvent>(&mut stream, N, Duration::from_millis(10_000)).await;
    assert_eq!(events.len(), N, "lagged-recovery must yield every durable event");
    // Cursors are monotonic.
    for pair in events.windows(2) {
        assert!(
            pair[1].cursor.as_bytes() > pair[0].cursor.as_bytes(),
            "cursors not monotonic across lagged-recovery window"
        );
    }
}
