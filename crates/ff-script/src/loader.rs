//! Library loader: ensures the `flowfabric` Lua function library is loaded
//! on the Valkey server at the expected version.
//!
//! Pattern: version-check → load-if-needed → verify (matches glide-mq).

use ferriskey::Client;

use crate::{LIBRARY_SOURCE, LIBRARY_VERSION};

/// Errors from the library loader.
#[derive(Debug)]
pub enum LoadError {
    /// ferriskey returned an error unrelated to "function not found".
    Valkey(ferriskey::Error),
    /// Version mismatch after load — another process may have loaded a
    /// different version concurrently.
    VersionMismatch { expected: String, got: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Valkey(e) => write!(f, "valkey error: {e}"),
            LoadError::VersionMismatch { expected, got } => {
                write!(f, "version mismatch after load: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Valkey(e) => Some(e),
            LoadError::VersionMismatch { .. } => None,
        }
    }
}

impl From<ferriskey::Error> for LoadError {
    fn from(e: ferriskey::Error) -> Self {
        LoadError::Valkey(e)
    }
}

/// Ensure the `flowfabric` function library is loaded on the server and at
/// the expected version.
///
/// 1. `FCALL ff_version 0` — check if library is loaded and version matches
/// 2. If missing or version mismatch → `FUNCTION LOAD REPLACE`
/// 3. Verify by calling `ff_version` again
pub async fn ensure_library(client: &Client) -> Result<(), LoadError> {
    match check_version(client).await {
        Ok(true) => {
            tracing::debug!("flowfabric library already loaded at version {LIBRARY_VERSION}");
            return Ok(());
        }
        Ok(false) => {
            tracing::info!("flowfabric library version mismatch, reloading");
        }
        Err(_) => {
            tracing::info!("flowfabric library not loaded, loading");
        }
    }

    // Load the library
    let _name: String = client.function_load_replace(LIBRARY_SOURCE).await?;

    // Verify
    match check_version(client).await {
        Ok(true) => {
            tracing::info!("flowfabric library loaded successfully (version {LIBRARY_VERSION})");
            Ok(())
        }
        Ok(false) => {
            let got = get_version_string(client).await.unwrap_or_default();
            Err(LoadError::VersionMismatch {
                expected: LIBRARY_VERSION.to_string(),
                got,
            })
        }
        Err(e) => Err(LoadError::Valkey(e)),
    }
}

/// Check if ff_version returns the expected version.
/// Returns Ok(true) if match, Ok(false) if mismatch, Err if function missing.
async fn check_version(client: &Client) -> Result<bool, ferriskey::Error> {
    let result: String = client
        .fcall("ff_version", &[] as &[&str], &[] as &[&str])
        .await?;
    Ok(result == LIBRARY_VERSION)
}

/// Get the version string for error reporting.
async fn get_version_string(client: &Client) -> Result<String, ferriskey::Error> {
    let result: String = client
        .fcall("ff_version", &[] as &[&str], &[] as &[&str])
        .await?;
    Ok(result)
}
