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
    // Dev-only override: `FF_FORCE_LUA_RELOAD=1` forces FUNCTION LOAD
    // REPLACE on every ensure_library call, skipping the version fast-
    // path. Addresses the dev/test footgun where local Lua edits
    // without a `flowfabric_lua_version` bump leave the cached library
    // stale (silent stale FCALLs — cost the PR-F cap-exceeded
    // investigation a session of red herrings before root-causing).
    //
    // The PR-level CI guardrail (`.github/workflows/matrix.yml` ff-
    // script lua drift) catches the same shape before merge; this
    // override covers the inner loop where a dev iterates locally
    // without pushing. Production deployments leave the env unset, so
    // the version-guard stays intact under concurrent deploys.
    //
    // Any truthy value (`1`, `true`, `yes`, case-insensitive) enables
    // the force-reload. Empty or unset is a no-op.
    let force_reload = std::env::var("FF_FORCE_LUA_RELOAD")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if force_reload {
        tracing::info!(
            "FF_FORCE_LUA_RELOAD set — bypassing version check, forcing FUNCTION LOAD REPLACE"
        );
    } else {
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
    // As of v0.12 the underlying race is handled inside ferriskey:
    // `FanOut + AllSucceeded` now triggers a topology refresh and
    // redrives the fan-out when a sub-request returns `READONLY`
    // (ferriskey/src/cluster/mod.rs, `OperationTarget::FanOut` arm).
    // That fix alone is sufficient for the tested cluster cells.
    //
    // This loader-side loop is kept for v0.12 as defense-in-depth:
    // the aggregate ferriskey retry bound is finite, and legitimate
    // cold-start topology churn can outlive it. It is scheduled to
    // be simplified/removed in v0.13 once the ferriskey fix has
    // proven sufficient across more CI runs. Each retry is still
    // productive (fresh topology), so the total window is ~14.5s
    // (500ms + 1s + 2s + 4s + 7s between 6 attempts).
    const MAX_ATTEMPTS: u32 = 6;
    let backoff_ms: [u64; 5] = [500, 1_000, 2_000, 4_000, 7_000];
    let mut last_err = None;
    for attempt in 1..=MAX_ATTEMPTS {
        let attempt_start = std::time::Instant::now();
        match client.function_load_replace(LIBRARY_SOURCE).await {
            Ok(_name) => {
                last_err = None;
                break;
            }
            Err(e) => {
                let attempt_ms = attempt_start.elapsed().as_millis() as u64;
                if is_permanent_load_error(&e) {
                    tracing::error!(attempt, attempt_ms, error = %e, "FUNCTION LOAD failed with permanent error");
                    return Err(LoadError::Valkey(e));
                }
                // Diagnostic instrumentation for the arm-cluster
                // FUNCTION LOAD READONLY flake (memory note:
                // project_arm_cluster_function_load_flake.md). On
                // ferriskey::ErrorKind::ReadOnly from a cluster client,
                // dump the canonical gossip-aggregated topology view
                // via CLUSTER NODES alongside the per-attempt
                // wall-clock so the next repro ships enough signal to
                // disambiguate: (a) client-side stale slot map vs
                // (b) cluster genuinely mid-gossip-convergence vs
                // (c) ferriskey's retry budget getting cut short at
                // 3 instead of the default 16.
                //
                // Gate: only runs on `ReadOnly`, so standalone dev
                // loops and permanent-syntax-error paths stay clean.
                // CLUSTER NODES is routed by ferriskey to a single
                // node (whichever served the last request); on arm
                // that's one of 7000-7005. A single-node answer is
                // enough — gossip-converged clusters agree; a
                // disagreement between that answer and ferriskey's
                // fan-out targets IS the signal we're looking for.
                if e.kind() == ferriskey::ErrorKind::ReadOnly {
                    let cluster_nodes: Result<String, ferriskey::Error> = client
                        .cmd("CLUSTER")
                        .arg("NODES")
                        .execute()
                        .await;
                    match cluster_nodes {
                        Ok(view) => tracing::warn!(
                            attempt,
                            attempt_ms,
                            error = %e,
                            cluster_nodes = %view,
                            "FUNCTION LOAD READONLY — canonical topology view (CLUSTER NODES) at failure"
                        ),
                        Err(probe_err) => tracing::warn!(
                            attempt,
                            attempt_ms,
                            error = %e,
                            probe_error = %probe_err,
                            "FUNCTION LOAD READONLY — CLUSTER NODES probe also failed"
                        ),
                    }
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
                        attempt_ms,
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

#[cfg(test)]
mod tests {
    //! Pin the classification of `is_permanent_load_error` against
    //! substring-match regressions. A previous debugging pass (see
    //! `feedback_verify_read_path_premises.md`) traced a production
    //! symptom to a near-miss in this family of classifiers. These tests
    //! lock in that transient server-directed errors (READONLY, MOVED,
    //! TRYAGAIN, CLUSTERDOWN) are NOT classified as permanent, so the
    //! loader retry path keeps working.
    use super::is_permanent_load_error;
    use ferriskey::{Error, ErrorKind};

    fn server_err(kind: ErrorKind, detail: &str) -> Error {
        // Matches the shape produced by ferriskey's RESP parser for
        // recognised server-error codes (see `protocol/parser.rs`
        // `parse_redis_error`). `server error` is the literal used
        // there.
        Error::from((kind, "server error", detail.to_string()))
    }

    #[test]
    fn readonly_is_not_permanent() {
        let e = server_err(
            ErrorKind::ReadOnly,
            "You can't write against a read only replica.",
        );
        assert!(
            !is_permanent_load_error(&e),
            "READONLY must be transient so loader retries on replica-claim races; \
             got permanent for {e}"
        );
    }

    #[test]
    fn moved_is_not_permanent() {
        let e = server_err(ErrorKind::Moved, "3999 127.0.0.1:7002");
        assert!(!is_permanent_load_error(&e), "MOVED must be transient; got {e}");
    }

    #[test]
    fn tryagain_is_not_permanent() {
        let e = server_err(ErrorKind::TryAgain, "resharding in progress");
        assert!(!is_permanent_load_error(&e), "TRYAGAIN must be transient; got {e}");
    }

    #[test]
    fn clusterdown_is_not_permanent() {
        let e = server_err(ErrorKind::ClusterDown, "The cluster is down");
        assert!(!is_permanent_load_error(&e), "CLUSTERDOWN must be transient; got {e}");
    }

    #[test]
    fn lua_syntax_error_is_permanent() {
        // Sanity check the positive side so we don't accidentally regress
        // the classifier to "always transient".
        let e = server_err(
            ErrorKind::ResponseError,
            "Error compiling function: user_script:12: syntax error near 'end'",
        );
        assert!(
            is_permanent_load_error(&e),
            "Lua compilation failure must be permanent; got transient for {e}"
        );
    }
}
