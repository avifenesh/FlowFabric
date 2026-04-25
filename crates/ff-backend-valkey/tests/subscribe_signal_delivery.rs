//! RFC-019 Stage B integration test — `subscribe_signal_delivery` on
//! the Valkey backend (#310).
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `localhost:6379`. Mirrors `subscribe_lease_history.rs`: subscribe
//! + synthetic XADD + observe.
//!
//! Run with:
//! ```
//! valkey-cli -h localhost -p 6379 ping
//! FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
//!   cargo test -p ff-backend-valkey --test subscribe_signal_delivery -- --ignored
//! ```

use std::time::Duration;

use ff_backend_valkey::{partition_signal_delivery_key, ValkeyBackend};
use ff_core::backend::BackendConfig;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::stream_subscribe::StreamCursor;
use futures_core::Stream;
use tokio::time::timeout;

async fn next_item<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_signal_delivery_yields_xadd_entry() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("localhost", 6379))
        .await
        .expect("connect to localhost valkey");

    let mut sub = backend
        .subscribe_signal_delivery(StreamCursor::empty())
        .await
        .expect("subscribe_signal_delivery");

    // Let the XREAD BLOCK loop register before we XADD.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let writer = ferriskey::ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .expect("build raw writer client");

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_signal_delivery_key(&partition);

    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key.as_str())
        .arg("*")
        .arg("signal_id")
        .arg("sig-rfc019-310")
        .arg("execution_id")
        .arg("exec-rfc019-310")
        .arg("waitpoint_id")
        .arg("wp-rfc019-310")
        .arg("effect")
        .arg("appended_to_waitpoint")
        .arg("delivered_at_ms")
        .arg("1700000000000")
        .execute()
        .await
        .expect("XADD synthetic signal-delivery event");

    let event = timeout(Duration::from_secs(10), next_item(&mut sub))
        .await
        .expect("subscription yielded within 10s")
        .expect("stream ended unexpectedly")
        .expect("backend surfaced error before event");

    assert_eq!(
        event.family,
        ff_core::stream_subscribe::StreamFamily::SignalDelivery,
        "event family mismatch"
    );
    assert!(
        !event.cursor.as_bytes().is_empty(),
        "event cursor should be concrete (non-empty)"
    );
    assert_eq!(
        event.cursor.as_bytes()[0],
        ff_core::stream_subscribe::VALKEY_CURSOR_PREFIX,
        "cursor prefix must be VALKEY_CURSOR_PREFIX (0x01)"
    );
    assert!(
        event
            .payload
            .windows(b"sig-rfc019-310".len())
            .any(|w| w == b"sig-rfc019-310"),
        "payload should contain the synthetic signal_id; got {:?}",
        event.payload
    );

    let _: ferriskey::Value = writer
        .cmd("DEL")
        .arg(stream_key.as_str())
        .execute()
        .await
        .unwrap_or(ferriskey::Value::Nil);
}
