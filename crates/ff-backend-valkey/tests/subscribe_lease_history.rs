//! RFC-019 Stage A integration test — `subscribe_lease_history` on the
//! Valkey backend.
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `localhost:6379` (the default deployment used by the project's
//! smoke gate). Run with:
//!
//! ```
//! valkey-cli -h localhost -p 6379 ping
//! cargo test -p ff-backend-valkey --test subscribe_lease_history -- --ignored
//! ```
//!
//! The test:
//! 1. Dials the backend + takes the `subscribe_lease_history` stream
//!    with an empty cursor ("tail from now").
//! 2. Writes a synthetic lease-history event via raw `XADD` on the
//!    partition-level aggregate stream key
//!    (`ff:part:{fp:0}:lease_history`).
//! 3. Asserts the subscription yields the event inside a 10-second
//!    window. Times out with a clear message otherwise so CI
//!    failures are obvious.

use std::time::Duration;

use ff_backend_valkey::{partition_lease_history_key, ValkeyBackend};
use ff_core::backend::BackendConfig;
use ff_core::partition::{Partition, PartitionFamily};
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
async fn subscribe_lease_history_yields_xadd_entry() {
    // Dial the backend. `BackendConfig::valkey("localhost", 6379)` is
    // the project-default smoke target; every in-tree live-Valkey
    // test uses the same shape.
    let backend = ValkeyBackend::connect(BackendConfig::valkey("localhost", 6379))
        .await
        .expect("connect to localhost valkey");

    // Open the subscription with an empty cursor → tail from now
    // (XREAD `$`). We then XADD a synthetic event and expect to
    // observe it.
    let mut sub = backend
        .subscribe_lease_history(StreamCursor::empty())
        .await
        .expect("subscribe_lease_history");

    // Give the XREAD BLOCK loop a moment to register before writing.
    // Without this the XADD can land before the subscriber advances
    // past `$`, which would strand the test on a no-event BLOCK
    // timeout.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Dial a *second* client just to XADD; `ValkeyBackend::connect`
    // returns an `Arc<dyn EngineBackend>` that does not expose the
    // raw ferriskey client, so the test opens its own connection.
    let writer = ferriskey::ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .expect("build raw writer client");

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_lease_history_key(&partition);

    // XADD <key> * event reclaimed lease_id test-lease-rfc019 prev_owner
    // test-worker at "unit-test"
    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key.as_str())
        .arg("*")
        .arg("event")
        .arg("reclaimed")
        .arg("lease_id")
        .arg("test-lease-rfc019")
        .arg("prev_owner")
        .arg("test-worker")
        .arg("at")
        .arg("unit-test")
        .execute()
        .await
        .expect("XADD synthetic lease-history event");

    // Observe the event via the subscription. 10s wall-clock budget
    // (well above the 5s XREAD BLOCK window + the 200ms sleep).
    let event = timeout(Duration::from_secs(10), next_item(&mut sub))
        .await
        .expect("subscription yielded within 10s")
        .expect("stream ended unexpectedly")
        .expect("backend surfaced error before event");

    assert_eq!(
        event.family,
        ff_core::stream_subscribe::StreamFamily::LeaseHistory,
        "event family mismatch"
    );
    assert!(
        !event.cursor.as_bytes().is_empty(),
        "event cursor should be concrete (non-empty)"
    );
    // Cursor must start with the Valkey family prefix.
    assert_eq!(
        event.cursor.as_bytes()[0],
        ff_core::stream_subscribe::VALKEY_CURSOR_PREFIX,
        "cursor prefix must be VALKEY_CURSOR_PREFIX (0x01)"
    );
    // Payload is an opaque NUL-delimited FIELD\0VALUE\0... blob in
    // Stage A; assert the event tag round-tripped by bytes-contains
    // (schema is Stage-B territory).
    assert!(
        event
            .payload
            .windows(b"reclaimed".len())
            .any(|w| w == b"reclaimed"),
        "payload should contain the synthetic `event=reclaimed` field; got {:?}",
        event.payload
    );

    // Cleanup: trim the synthetic entry so the next run starts clean.
    let _: ferriskey::Value = writer
        .cmd("DEL")
        .arg(stream_key.as_str())
        .execute()
        .await
        .unwrap_or(ferriskey::Value::Nil);
}
