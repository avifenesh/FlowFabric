//! RFC-019 Stage B integration test — `subscribe_completion` on the
//! Valkey backend (issue #309).
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `localhost:6379`. Run with:
//!
//! ```
//! valkey-cli -h localhost -p 6379 ping
//! cargo test -p ff-backend-valkey --test subscribe_completion -- --ignored
//! ```
//!
//! The test:
//! 1. Dials the backend + opens `subscribe_completion` with an empty
//!    cursor (the only cursor Valkey Stage B accepts — pubsub-backed).
//! 2. Publishes a synthetic completion payload on `ff:dag:completions`
//!    (the channel the Lua emitters + `CompletionBackend` plumb
//!    through). Matching the same surface via raw PUBLISH keeps the
//!    test backend-surface-only + avoids dragging in `ff_script`.
//! 3. Asserts the subscription yields the event inside a 5-second
//!    window with `family = Completion`, empty cursor, and the
//!    execution-id round-trips.

use std::time::Duration;

use ff_backend_valkey::{ValkeyBackend, COMPLETION_CHANNEL};
use ff_core::backend::{BackendConfig, ScannerFilter};
use ff_core::stream_events::CompletionOutcome;
use ff_core::stream_subscribe::StreamCursor;
use futures_core::Stream;
use tokio::time::timeout;

/// Poll `.next()` on a pinned stream without dragging in `futures`.
async fn next_item<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_completion_yields_publish_frame() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("localhost", 6379))
        .await
        .expect("connect to localhost valkey");

    let mut sub = backend
        .subscribe_completion(StreamCursor::empty(), &ScannerFilter::default())
        .await
        .expect("subscribe_completion");

    // Give the RESP3 SUBSCRIBE handshake a moment to land — without
    // this the PUBLISH below can race ahead of the subscriber and the
    // test strands on a no-event poll.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Dial a separate client just to PUBLISH. `ValkeyBackend::connect`
    // returns a backend handle that does not expose the raw ferriskey
    // client; opening our own client matches `subscribe_lease_history`
    // test pattern.
    let publisher = ferriskey::ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .expect("build raw publisher client");

    // Wire shape matches `completion::WirePayload`: execution_id in
    // `{fp:N}:<uuid>` form, bare flow_id uuid, outcome string.
    // Use fixed uuids so the assertion is deterministic.
    let exec_id = "{fp:0}:11111111-1111-4111-8111-111111111111";
    let flow_id = "22222222-2222-4222-8222-222222222222";
    let payload = format!(
        r#"{{"execution_id":"{exec_id}","flow_id":"{flow_id}","outcome":"success"}}"#
    );

    let _: ferriskey::Value = publisher
        .cmd("PUBLISH")
        .arg(COMPLETION_CHANNEL)
        .arg(payload.as_str())
        .execute()
        .await
        .expect("PUBLISH synthetic completion");

    let event = timeout(Duration::from_secs(5), next_item(&mut sub))
        .await
        .expect("subscription yielded within 5s")
        .expect("stream ended unexpectedly")
        .expect("backend surfaced error before event");

    assert!(
        event.cursor.as_bytes().is_empty(),
        "Stage B cursor must be the empty sentinel (pubsub-backed, non-durable)"
    );
    assert_eq!(
        event.execution_id.to_string(),
        exec_id,
        "execution_id should round-trip"
    );
    assert_eq!(
        event.outcome,
        CompletionOutcome::Success,
        "outcome should typed-decode to Success"
    );
}
