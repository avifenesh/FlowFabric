//! FlowFabric — umbrella crate re-exporting the published crate family.
//!
//! Pin one crate; let Cargo resolve the rest. Closes #279 — prior to
//! v0.8.2 consumers (cairn-fabric, downstream integrations) had to
//! pin all 7–8 `ff-*` crates + `ferriskey` in lockstep and version-
//! skew drift was only caught at `cargo update` time. This crate is
//! the single-line import surface; feature flags gate which backend
//! and which optional internals get pulled in.
//!
//! # Quick start
//!
//! ```toml
//! # Valkey backend (default):
//! flowfabric = "0.8"
//!
//! # Postgres backend (explicit, no valkey):
//! flowfabric = { version = "0.8", default-features = false, features = ["postgres"] }
//!
//! # Advanced — direct engine/scheduler access:
//! flowfabric = { version = "0.8", features = ["engine", "scheduler-internals"] }
//! ```
//!
//! See `docs/CONSUMER_MIGRATION_v0.8.md` § "Umbrella crate
//! (flowfabric)" for mapping from the 7-crate import shape to the
//! umbrella shape.
//!
//! # Re-export map
//!
//! | Umbrella path | Underlying crate | Feature gate |
//! |---|---|---|
//! | `flowfabric::core`      | `ff_core`              | always |
//! | `flowfabric::sdk`       | `ff_sdk`               | always |
//! | `flowfabric::valkey`    | `ff_backend_valkey`    | `valkey` (default) |
//! | `flowfabric::postgres`  | `ff_backend_postgres`  | `postgres` |
//! | `flowfabric::engine`    | `ff_engine`            | `engine` |
//! | `flowfabric::scheduler` | `ff_scheduler`         | `scheduler-internals` |
//! | `flowfabric::script`    | `ff_script`            | `script-internals` or `valkey` |
//!
//! The [`prelude`] module glob-re-exports the 80% path
//! (`flowfabric::sdk::*`), which already flattens the most-used
//! `ff_core::contracts` and `ff_core::backend` types via `ff-sdk`'s
//! own `pub use`. Consumers needing scheduler/engine types should
//! enable the respective feature flag and reach through
//! `flowfabric::engine::…` / `flowfabric::scheduler::…` explicitly.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub use ff_core as core;
pub use ff_sdk as sdk;

#[cfg(feature = "valkey")]
#[cfg_attr(docsrs, doc(cfg(feature = "valkey")))]
pub use ff_backend_valkey as valkey;

#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
pub use ff_backend_postgres as postgres;

#[cfg(feature = "engine")]
#[cfg_attr(docsrs, doc(cfg(feature = "engine")))]
pub use ff_engine as engine;

#[cfg(feature = "scheduler-internals")]
#[cfg_attr(docsrs, doc(cfg(feature = "scheduler-internals")))]
pub use ff_scheduler as scheduler;

// `ff_script` is pulled in by either the `script-internals` opt-in or
// transitively by the default `valkey` feature (valkey-default's
// FCALL path depends on it). Either way, re-expose it at
// `flowfabric::script` so consumers don't have to guess which flag
// to toggle.
#[cfg(any(feature = "script-internals", feature = "valkey"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "script-internals", feature = "valkey"))))]
pub use ff_script as script;

/// Prelude — convenience glob re-export of the most-used SDK surface.
///
/// `ff_sdk` already re-exports the common `ff_core::contracts` and
/// `ff_core::backend` types (e.g. `ClaimGrant`, `FailOutcome`,
/// `ResumeSignal`, `SummaryDocument`), so a single
/// `use flowfabric::prelude::*;` covers the 80% consumer code path.
///
/// Scheduler/engine/backend types are intentionally NOT flattened
/// here — consumers that need them enable the feature flag and
/// reach through the qualified path (`flowfabric::engine::…`,
/// `flowfabric::scheduler::…`, `flowfabric::valkey::…`). This keeps
/// prelude imports stable across feature-flag configurations.
pub mod prelude {
    pub use crate::sdk::*;
}
