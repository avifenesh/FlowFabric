//! Retry classification for `ferriskey::ErrorKind`.
//!
//! Shared by ff-server, ff-scheduler, ff-sdk so a single table decides
//! what's retryable. Kept here (not ff-core) because `ferriskey::ErrorKind`
//! lives in the transport client; ff-core is kept transport-free.

/// Classify a ferriskey `ErrorKind` as retryable by the caller.
///
/// Returns `true` for kinds that are **known-safe** to retry:
/// - `IoError`, `FatalSendError`: the request never reached the server.
/// - `TryAgain`: Valkey explicitly asked for a retry.
/// - `BusyLoadingError`: Valkey is booting; transient.
/// - `ClusterDown`: cluster is rebalancing; transient.
///
/// Returns `false` for:
/// - `FatalReceiveError`: the request **may have been applied** server-side
///   but the response was lost. Treated as non-retryable by default because
///   ff-server cannot know if the operation was idempotent. Callers that
///   know the operation is idempotent may retry anyway, but this helper
///   errs on the safe side.
/// - `Moved` / `Ask`: ferriskey handles cluster redirects internally; if
///   they surface to the caller it means the redirect chain already failed,
///   and another caller-level retry will hit the same wall.
/// - `AuthenticationFailed` / `PermissionDenied` / `InvalidClientConfig`:
///   config mismatch, not transient.
/// - `NoScriptError`: `fcall_with_reload` already did the reload-retry
///   internally; if we surface NOSCRIPT to the caller, the library is
///   missing even after reload.
/// - Any other kind: conservative default false.
pub fn is_retryable_kind(kind: ferriskey::ErrorKind) -> bool {
    use ferriskey::ErrorKind::*;
    matches!(
        kind,
        IoError | FatalSendError | TryAgain | BusyLoadingError | ClusterDown
    )
}

/// Map a ferriskey `ErrorKind` to a stable, snake_case wire string.
///
/// Used in HTTP `ErrorBody.kind` and any other external API so callers can
/// dispatch on a string contract without depending on the `Debug` repr of
/// `ferriskey::ErrorKind` (which is a library-internal formatting choice
/// that may change across ferriskey versions).
///
/// Contract: these strings are part of the public wire API. Do not rename
/// without bumping a major version. New `ErrorKind` variants added upstream
/// map to `"unknown"` until explicitly handled here.
pub fn kind_to_stable_str(kind: ferriskey::ErrorKind) -> &'static str {
    use ferriskey::ErrorKind::*;
    match kind {
        ResponseError => "response_error",
        ParseError => "parse_error",
        AuthenticationFailed => "authentication_failed",
        PermissionDenied => "permission_denied",
        TypeError => "type_error",
        ExecAbortError => "exec_abort",
        BusyLoadingError => "busy_loading",
        NoScriptError => "no_script",
        InvalidClientConfig => "invalid_client_config",
        Moved => "moved",
        Ask => "ask",
        TryAgain => "try_again",
        ClusterDown => "cluster_down",
        CrossSlot => "cross_slot",
        MasterDown => "master_down",
        IoError => "io_error",
        FatalSendError => "fatal_send",
        FatalReceiveError => "fatal_receive",
        ClientError => "client_error",
        ExtensionError => "extension_error",
        ReadOnly => "read_only",
        MasterNameNotFoundBySentinel => "master_name_not_found_by_sentinel",
        NoValidReplicasFoundBySentinel => "no_valid_replicas_found_by_sentinel",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;

    #[test]
    fn retryable_kinds_whitelist() {
        assert!(is_retryable_kind(ErrorKind::IoError));
        assert!(is_retryable_kind(ErrorKind::FatalSendError));
        assert!(is_retryable_kind(ErrorKind::TryAgain));
        assert!(is_retryable_kind(ErrorKind::BusyLoadingError));
        assert!(is_retryable_kind(ErrorKind::ClusterDown));
    }

    #[test]
    fn non_retryable_kinds() {
        assert!(!is_retryable_kind(ErrorKind::FatalReceiveError));
        assert!(!is_retryable_kind(ErrorKind::AuthenticationFailed));
        assert!(!is_retryable_kind(ErrorKind::PermissionDenied));
        assert!(!is_retryable_kind(ErrorKind::InvalidClientConfig));
        assert!(!is_retryable_kind(ErrorKind::NoScriptError));
        assert!(!is_retryable_kind(ErrorKind::Moved));
        assert!(!is_retryable_kind(ErrorKind::Ask));
        assert!(!is_retryable_kind(ErrorKind::ResponseError));
        assert!(!is_retryable_kind(ErrorKind::ParseError));
        assert!(!is_retryable_kind(ErrorKind::TypeError));
        assert!(!is_retryable_kind(ErrorKind::ReadOnly));
    }

    #[test]
    fn stable_str_for_common_kinds() {
        assert_eq!(kind_to_stable_str(ErrorKind::IoError), "io_error");
        assert_eq!(kind_to_stable_str(ErrorKind::FatalSendError), "fatal_send");
        assert_eq!(kind_to_stable_str(ErrorKind::FatalReceiveError), "fatal_receive");
        assert_eq!(kind_to_stable_str(ErrorKind::NoScriptError), "no_script");
        assert_eq!(kind_to_stable_str(ErrorKind::AuthenticationFailed), "authentication_failed");
        assert_eq!(kind_to_stable_str(ErrorKind::ClusterDown), "cluster_down");
        assert_eq!(kind_to_stable_str(ErrorKind::Moved), "moved");
        assert_eq!(kind_to_stable_str(ErrorKind::ReadOnly), "read_only");
    }

    #[test]
    fn stable_str_is_snake_case_and_nonempty() {
        use ferriskey::ErrorKind::*;
        let all = [
            ResponseError, ParseError, AuthenticationFailed, PermissionDenied,
            TypeError, ExecAbortError, BusyLoadingError, NoScriptError,
            InvalidClientConfig, Moved, Ask, TryAgain, ClusterDown, CrossSlot,
            MasterDown, IoError, FatalSendError, FatalReceiveError, ClientError,
            ExtensionError, ReadOnly,
        ];
        for k in all {
            let s = kind_to_stable_str(k);
            assert!(!s.is_empty(), "empty stable str for {k:?}");
            assert!(
                s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "non snake_case stable str '{s}' for {k:?}"
            );
        }
    }
}
