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

    // Load the library with retry for transient errors.
    //
    // Cluster-topology race (issues #275 / #369): on a just-formed
    // cluster, `FUNCTION LOAD` can route to a node that's a replica
    // at dispatch time (gossip hasn't fully converged, or ferriskey's
    // cached slot map pre-dates the primary/replica swap). Valkey
    // rejects with `READONLY: You can't write against a read only
    // replica.`
    //
    // Mitigation: before each retry, force an unthrottled cluster
    // slot-map refresh via ferriskey, then back off briefly. Each
    // retry is productive (fresh topology), so the total window is
    // ~14.5s (500ms + 1s + 2s + 4s + 7s between 6 attempts) instead
    // of the earlier ~39s blind-sleep window.
    //
    // A deeper fix — ferriskey auto-refreshing on `READONLY` inside
    // its cluster dispatch — is tracked as a follow-up to #290.
    const MAX_ATTEMPTS: u32 = 6;
    let backoff_ms: [u64; 5] = [500, 1_000, 2_000, 4_000, 7_000];
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
                if attempt < MAX_ATTEMPTS {
                    // Log the transient error's Display before moving
                    // `e` into `last_err` so the structured field is
                    // a real error reference, not a post-hoc
                    // `Option<String>` round-trip.
                    let backoff = backoff_ms
                        .get((attempt as usize).saturating_sub(1))
                        .copied()
                        .unwrap_or(4_000);
                    // Before sleeping, force a slot-map refresh so the
                    // next attempt routes against fresh topology. No-op
                    // in standalone mode. A refresh failure is logged
                    // but does not abort the retry loop — the sleep +
                    // next attempt may still succeed (e.g. after a
                    // background topology tick).
                    if let Err(refresh_err) = client.force_cluster_slot_refresh().await {
                        tracing::debug!(
                            attempt,
                            error = %refresh_err,
                            "force_cluster_slot_refresh failed between FUNCTION LOAD retries"
                        );
                    }
                    tracing::warn!(
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        backoff_ms = backoff,
                        error = %e,
                        "FUNCTION LOAD failed (transient), refreshed slot map, retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                } else {
                    last_err = Some(e);
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
