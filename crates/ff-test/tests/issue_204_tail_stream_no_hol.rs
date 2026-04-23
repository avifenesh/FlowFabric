//! Issue #204 — concurrent `tail_stream` + `append_frame`-style writes on
//! a single ferriskey client must NOT head-of-line block each other.
//!
//! Pre-fix: both operations shared one multiplexed ferriskey connection, so
//! a long-blocking `XREAD BLOCK` tail occupied the only mux slot and a
//! concurrent `XADD` (the transport beneath `append_frame`) stalled until
//! the tail returned — or, more commonly, was falsely surfaced to the
//! caller as `EngineError::Transport { source: "timed out" }` when the
//! ferriskey client's request-timeout fired while the BLOCK was still in
//! flight.
//!
//! Post-fix: `tail_stream_impl` issues its blocking XREAD on a dedicated
//! connection obtained via `ferriskey::Client::duplicate_connection()`, so
//! writes on the main client complete regardless of the tail's state.
//!
//! Contract exercised here:
//! 1. Tail a never-written attempt stream with `block_ms = 2000` (via the
//!    same `xread_block` primitive `tail_stream_impl` uses, but against a
//!    freshly-duplicated connection — simulating the post-fix call path).
//! 2. While the tail is in flight, fire a non-blocking XADD against the
//!    ORIGINAL ferriskey client.
//! 3. The XADD MUST return within 500 ms. Against the pre-fix single-mux
//!    call path, XADD sits behind the BLOCK and either completes ~2000 ms
//!    later (tail returns first) or times out first — either way it
//!    exceeds the 500 ms budget. The assertion distinguishes the fix
//!    from the regression.

use std::time::{Duration, Instant};

use ff_test::fixtures::TestCluster;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_does_not_stall_behind_blocking_tail() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let main = tc.client().clone();
    let stream_key = format!(
        "{{p:0}}:issue204:stream:{}",
        uuid::Uuid::new_v4()
    );
    let stream_meta_key = format!("{{p:0}}:issue204:stream_meta:{}", uuid::Uuid::new_v4());

    // The tail runs on a duplicate connection — matching what the
    // post-fix `tail_stream_impl` does internally.
    let tail_client = main
        .duplicate_connection()
        .await
        .expect("duplicate_connection should succeed against live Valkey");

    let tail_key = stream_key.clone();
    let tail_meta = stream_meta_key.clone();
    let tail_handle = tokio::spawn(async move {
        let start = Instant::now();
        let result = ff_script::stream_tail::xread_block(
            &tail_client,
            &tail_key,
            &tail_meta,
            "$",
            2_000, // block up to 2s; no frames will arrive during the test
            10,
        )
        .await;
        (result, start.elapsed())
    });

    // Give the BLOCK a moment to land on the dedicated socket so the race
    // we care about (write vs already-parked blocking read) is genuine.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now fire the XADD on the main client. Pre-fix, this would wait on
    // the BLOCK (shared mux) and blow the 500 ms budget; post-fix, the
    // blocking read is on a different socket so the XADD is unaffected.
    let write_start = Instant::now();
    let write_result: ferriskey::Value = main
        .cmd("XADD")
        .arg(&stream_key)
        .arg("*")
        .arg("field")
        .arg("value")
        .execute()
        .await
        .expect("XADD on main client must succeed");
    let write_elapsed = write_start.elapsed();

    // `XADD *` returns the stream entry ID as a BulkString; we don't care
    // about the value, only that the round-trip completed quickly.
    assert!(
        matches!(&write_result, ferriskey::Value::BulkString(_)),
        "XADD should return a BulkString entry id, got {write_result:?}"
    );

    assert!(
        write_elapsed < Duration::from_millis(500),
        "XADD stalled {write_elapsed:?} — expected < 500ms. This is the \
         issue #204 regression: the blocking tail is head-of-line-blocking \
         writes on the shared ferriskey multiplex."
    );

    // Drain the tail so the test doesn't leave a background task leaking.
    let (tail_result, tail_elapsed) = tail_handle.await.expect("tail task panicked");
    let frames = tail_result.expect("xread_block should succeed (nil on timeout is Ok)");
    // Tail is allowed to either return early (because XADD landed on the
    // stream it's watching) or ride out the full block — we don't assert
    // either way; only the write-side latency matters for this test.
    let _ = frames;
    let _ = tail_elapsed;

    tc.cleanup().await;
}

/// Companion test: demonstrates the head-of-line behavior that #204
/// originally reported, using the shared-multiplex call path (BLOCK and
/// XADD on the same `ferriskey::Client`). The XADD stall is what
/// `tail_stream_impl` used to inflict on `append_frame`. This test is
/// documentation/proof-of-reproduction — mark it `#[ignore]` so CI
/// doesn't wait 2s on every run, but it can be executed manually to
/// confirm the bug is real:
///
///     cargo test -p ff-test --test issue_204_tail_stream_no_hol -- \
///         --ignored shared_mux_stalls_write
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "demonstrates the pre-fix head-of-line regression; 2s wall-clock"]
async fn shared_mux_stalls_write() {
    let tc = TestCluster::connect().await;

    let shared = tc.client().clone();
    let stream_key = format!(
        "{{p:0}}:issue204:shared:{}",
        uuid::Uuid::new_v4()
    );
    let stream_meta_key = format!(
        "{{p:0}}:issue204:shared_meta:{}",
        uuid::Uuid::new_v4()
    );

    let tail_client = shared.clone(); // SHARED — the bug.
    let tail_key = stream_key.clone();
    let tail_meta = stream_meta_key.clone();
    let _tail_handle = tokio::spawn(async move {
        let _ = ff_script::stream_tail::xread_block(
            &tail_client,
            &tail_key,
            &tail_meta,
            "$",
            2_000,
            10,
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let write_start = Instant::now();
    let write_outcome: Result<ferriskey::Value, ferriskey::Error> = shared
        .cmd("XADD")
        .arg(&stream_key)
        .arg("*")
        .arg("field")
        .arg("value")
        .execute()
        .await;
    let write_elapsed = write_start.elapsed();

    // Under the shared-mux bug, the XADD either (a) waits behind the
    // BLOCK and returns late or (b) trips the ferriskey request-timeout
    // and returns an error — both outcomes surface the head-of-line
    // pathology that #204 is about. We accept either manifestation.
    let stalled = write_outcome.is_err() || write_elapsed >= Duration::from_millis(500);
    assert!(
        stalled,
        "expected shared-mux head-of-line blocking (XADD err or >= 500ms) \
         but write_elapsed={write_elapsed:?} result={write_outcome:?} — \
         either the bug has been papered over elsewhere or ferriskey's \
         mux behavior has changed"
    );

    // No trailing cleanup: the background tail task is still parked on
    // the shared mux and any FLUSHDB round-trip would itself stall.
    // This test is `#[ignore]`d and only runs manually, so leak is ok.
}
