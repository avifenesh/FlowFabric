//! FlowFabric server — HTTP API, Valkey connection, and background engine.

pub mod admin;
pub mod api;
pub mod config;
pub mod server;

pub use config::ServerConfig;
pub use server::{Server, ServerError};
