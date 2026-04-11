// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

#[cfg(feature = "mimalloc-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(feature = "jemalloc", not(feature = "mimalloc-alloc")))]
#[global_allocator]
static GLOBAL_JE: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[macro_use]
pub mod valkey;

pub mod client;
pub mod otel_db_semantics;
pub mod compression;
pub mod errors;
pub mod scripts_container;
pub use client::ConnectionRequest;
pub mod cluster_scan_container;
pub mod iam;
pub mod pubsub;
pub mod request_type;
pub use telemetrylib::{
    DEFAULT_FLUSH_SIGNAL_INTERVAL_MS, DEFAULT_TRACE_SAMPLE_PERCENTAGE, FerrisKeyOtel,
    FerrisKeyOtelConfigBuilder, OtelSignalsExporter, FerrisKeySpan, Telemetry,
};
