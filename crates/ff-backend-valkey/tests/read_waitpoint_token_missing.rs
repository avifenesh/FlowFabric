//! Issue #434 — `EngineBackend::read_waitpoint_token` on Valkey.
//!
//! `Ok(None)` for a waitpoint that was never written: HGET on a
//! missing hash key returns `nil`, which the impl maps to `None`.
//! This is the signal-bridge's graceful-degradation path — a signal
//! may arrive after its waitpoint has been consumed or expired, and
//! a missing waitpoint is not an error on that path.
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379 required), matching
//! the sibling live tests in `capabilities.rs` +
//! `read_execution_context_missing.rs`.

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::WaitpointId;

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn read_waitpoint_token_missing_returns_none() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend");

    // Mint a fresh waitpoint id that was never written to the
    // backend, and an arbitrary partition. HGET nil → Ok(None).
    let partition = PartitionKey::from(&Partition {
        family: PartitionFamily::Flow,
        index: 0,
    });
    let wp_id = WaitpointId::new();

    let out = backend
        .read_waitpoint_token(partition, &wp_id)
        .await
        .expect("missing waitpoint must be Ok(None), not an error");
    assert!(out.is_none(), "expected None for missing waitpoint, got {out:?}");
}
