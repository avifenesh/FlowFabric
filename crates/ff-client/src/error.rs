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
}

impl ClientError {
    pub(crate) fn missing_field(name: &'static str) -> Self {
        ClientError::MissingField(name)
    }

    pub(crate) fn backend_not_yet_supported(name: &'static str) -> Self {
        ClientError::BackendNotYetSupported(name)
    }
}
