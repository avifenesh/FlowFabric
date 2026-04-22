//! FlowFabric server — HTTP API, Valkey connection, and background engine.

pub mod admin;
pub mod api;
pub mod config;
pub mod metrics;
pub mod server;

pub use config::ServerConfig;
pub use metrics::Metrics;
pub use server::{Server, ServerError};
