//! FlowFabric Worker SDK — public API for worker authors.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use ff_sdk::{FlowFabricWorker, WorkerConfig};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), ff_sdk::SdkError> {
//!     let config = WorkerConfig::new(
//!         "localhost",
//!         6379,
//!         "my-worker",
//!         "my-worker-instance-1",
//!         "default",
//!         "main",
//!     );
//!
//!     let worker = FlowFabricWorker::connect(config).await?;
//!
//!     loop {
//!         match worker.claim_next().await? {
//!             Some(task) => {
//!                 println!("claimed: {}", task.execution_id());
//!                 // Process task...
//!                 task.complete(Some(b"done".to_vec())).await?;
//!             }
//!             None => {
//!                 tokio::time::sleep(Duration::from_secs(1)).await;
//!             }
//!         }
//!     }
//! }
//! ```

pub mod config;
pub mod task;
pub mod worker;

// Re-exports for convenience
pub use config::WorkerConfig;
pub use task::{
    AppendFrameOutcome, ClaimedTask, ConditionMatcher, FailOutcome, Signal, SignalOutcome,
    SuspendOutcome, TimeoutBehavior,
};
pub use worker::FlowFabricWorker;

/// SDK error type.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// Valkey connection or command error.
    #[error("valkey: {0}")]
    Valkey(String),

    /// FlowFabric Lua script error.
    #[error("script: {0}")]
    Script(#[from] ff_core::error::ScriptError),

    /// Configuration error.
    #[error("config: {0}")]
    Config(String),
}
