use thiserror::Error;

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("missing required builder field: {0}")]
    MissingField(&'static str),

    #[error("backend not yet supported in ff-client: {0}")]
    BackendNotYetSupported(&'static str),

    #[error("transport error: {0}")]
    Transport(#[from] ferriskey::Error),

    /// Wraps any [`EngineError`](ff_core::engine_error::EngineError)
    /// returned by a backend op (e.g. `create_execution`). Consumers
    /// that need the typed inner shape can match on the inner enum.
    #[error("engine error: {0}")]
    Engine(#[from] ff_core::engine_error::EngineError),

    /// Lua library load failure during [`crate::ClientBuilder::build`].
    /// Surfaces `ff_script::loader::LoadError` so the user can tell a stale
    /// Valkey topology from a permission denial.
    #[error("lua library load failed: {0}")]
    ScriptLoad(#[from] ff_script::loader::LoadError),
}

impl ClientError {
    pub(crate) fn missing_field(name: &'static str) -> Self {
        ClientError::MissingField(name)
    }

    pub(crate) fn backend_not_yet_supported(name: &'static str) -> Self {
        ClientError::BackendNotYetSupported(name)
    }

    pub(crate) fn script_load(e: ff_script::loader::LoadError) -> Self {
        ClientError::ScriptLoad(e)
    }
}
