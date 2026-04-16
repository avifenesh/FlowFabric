//! Library loader: ensures the `flowfabric` Lua function library is loaded
//! on the Valkey server at the expected version.
//!
//! Pattern: version-check → load-if-needed → verify (matches glide-mq).

use ferriskey::Client;

use crate::{LIBRARY_SOURCE, LIBRARY_VERSION};

/// Errors from the library loader.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// ferriskey returned an error unrelated to "function not found".
    /// Preserves `ferriskey::ErrorKind` so callers can distinguish
    /// timeout / auth / NOSCRIPT / cluster-redirect on library-load failures.
    #[error("valkey: {0}")]
    Valkey(#[from] ferriskey::Error),
    /// Version mismatch after load — another process may have loaded a
    /// different version concurrently.
    #[error("version mismatch after load: expected {expected}, got {got}")]
    VersionMismatch { expected: String, got: String },
}

impl LoadError {
    /// Returns the underlying ferriskey ErrorKind, if this is a Valkey error.
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Valkey(e) => Some(e.kind()),
            _ => None,
        }
    }
}

/// Ensure the `flowfabric` function library is loaded on the server and at
/// the expected version.
///
/// 1. `FCALL ff_version 0` — check if library is loaded and version matches
/// 2. If missing or version mismatch → `FUNCTION LOAD REPLACE` (with retry)
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

    // Load the library with retry for transient errors
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match client.function_load_replace(LIBRARY_SOURCE).await {
            Ok(_name) => {
                last_err = None;
                break;
            }
            Err(e) => {
                if is_permanent_load_error(&e) {
                    tracing::error!(attempt, error = %e, "FUNCTION LOAD failed with permanent error");
                    return Err(LoadError::Valkey(e));
                }
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    error = %e,
                    "FUNCTION LOAD failed (transient), retrying in 1s"
                );
                last_err = Some(e);
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(LoadError::Valkey(e));
    }

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

/// Check if a FUNCTION LOAD error is permanent (syntax error in Lua) vs transient.
fn is_permanent_load_error(e: &ferriskey::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Error compiling") || msg.contains("syntax error") || msg.contains("ERR Error")
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
