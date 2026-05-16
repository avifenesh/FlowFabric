//! FlowFabric client SDK.
//!
//! Public API: [`Client`] + [`ClientBuilder`].
//!
//! Backends are an implementation detail. Today only the Valkey backend is
//! wired (via `ferriskey` internally); Postgres and Sqlite variants of
//! [`BackendConfig`] are accepted by the builder but currently fail at
//! `build()` with [`ClientError::BackendNotYetSupported`]. The builder
//! surface is forward-compatible so adding those impls is additive.
//!
//! `ferriskey` is an internal transport — never re-exported. Consumers that
//! need a generic Valkey client should use a Valkey client library
//! (e.g. valkey-glide) directly; this crate is for FlowFabric operations.

mod builder;
mod cancel;
mod client;
mod error;
mod inspect;
mod read_context;
mod submit;

pub use ff_core::backend::{BackendConfig, BackendConnection, BackendTag};
pub use ff_core::contracts::{ExecutionContext, ExecutionSnapshot};
pub use ff_core::types::{ExecutionId, LaneId, Namespace};

pub use builder::ClientBuilder;
pub use cancel::CancelResult;
pub use client::Client;
pub use error::{ClientError, Result};
pub use inspect::{AttemptSummary, LeaseSummary, PublicState};
pub use submit::{SubmitRequest, SubmitResult};
