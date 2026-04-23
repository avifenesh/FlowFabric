//! ferriskey → [`BackendError`] conversion (#88).
//!
//! This is the single boundary where a native `ferriskey::Error` is
//! lifted into the backend-agnostic [`BackendError`] surfaced by
//! ff-sdk and ff-server. Keeping the conversion here — not in ff-core
//! — preserves ff-core's ferriskey-free public surface. Every
//! ff-sdk/ff-server constructor that previously wrapped
//! `ferriskey::Error` directly now goes through these helpers.
//!
//! The mapping collapses ferriskey's 30+ `ErrorKind` variants into
//! the curated [`BackendErrorKind`] taxonomy. The `message` field
//! preserves the `Display` rendering of the native error so operator
//! diagnostics do not regress.

use ff_core::engine_error::{BackendError, BackendErrorKind};

/// Classify a ferriskey [`ErrorKind`] into the backend-agnostic
/// [`BackendErrorKind`]. See the ferriskey `value.rs` enum for the
/// full source taxonomy; the buckets here are grouped by
/// consumer-meaningful retry / operator posture, not 1:1 with
/// ferriskey's internal categorisation.
///
/// [`ErrorKind`]: ferriskey::ErrorKind
pub fn classify_ferriskey_kind(kind: ferriskey::ErrorKind) -> BackendErrorKind {
    use ferriskey::ErrorKind as K;
    match kind {
        // Transport / I/O family.
        K::IoError | K::FatalSendError | K::FatalReceiveError | K::ProtocolDesync => {
            BackendErrorKind::Transport
        }
        // Cluster topology churn.
        K::Moved
        | K::Ask
        | K::TryAgain
        | K::ClusterDown
        | K::CrossSlot
        | K::MasterDown
        | K::AllConnectionsUnavailable
        | K::ConnectionNotFoundForRoute
        | K::NotAllSlotsCovered
        | K::MasterNameNotFoundBySentinel
        | K::NoValidReplicasFoundBySentinel => BackendErrorKind::Cluster,
        // Auth.
        K::AuthenticationFailed | K::PermissionDenied => BackendErrorKind::Auth,
        // Backend-side loading churn.
        K::BusyLoadingError => BackendErrorKind::BusyLoading,
        // Script / function lifecycle.
        K::NoScriptError => BackendErrorKind::ScriptNotLoaded,
        // Protocol-level rejections (malformed response, wrong type,
        // client-side config issue, ExtensionError / UserOperationError,
        // etc.).
        K::ResponseError
        | K::ParseError
        | K::TypeError
        | K::ExecAbortError
        | K::InvalidClientConfig
        | K::ClientError
        | K::ExtensionError
        | K::ReadOnly
        | K::EmptySentinelList
        | K::NotBusy
        | K::RESP3NotSupported
        | K::UserOperationError => BackendErrorKind::Protocol,
        // ferriskey::ErrorKind is not `#[non_exhaustive]` today, but
        // guard anyway — a future variant lands in a terminal
        // `Other` bucket rather than failing to compile.
        #[allow(unreachable_patterns)]
        _ => BackendErrorKind::Other,
    }
}

/// Lift a ferriskey error into a [`BackendError::Valkey`]. Preserves
/// the native `Display` rendering as `message`.
pub fn backend_error_from_ferriskey(err: &ferriskey::Error) -> BackendError {
    BackendError::Valkey {
        kind: classify_ferriskey_kind(err.kind()),
        message: err.to_string(),
    }
}

/// `From` shim for ergonomic `?`-propagation. Consumes the native
/// error; callers that need to retain the ferriskey side (for trace
/// logging) should call [`backend_error_from_ferriskey`] before
/// dropping the original.
impl From<ferriskey::Error> for BackendErrorWrapper {
    fn from(err: ferriskey::Error) -> Self {
        Self(backend_error_from_ferriskey(&err))
    }
}

/// Transparent newtype so the `From<ferriskey::Error>` impl is
/// orphan-rule–legal inside this crate without imposing on ff-core's
/// foreign [`BackendError`] type. Consumers call `.into_inner()` to
/// recover the [`BackendError`]; this stays an implementation detail
/// of ff-sdk / ff-server boundaries.
pub struct BackendErrorWrapper(BackendError);

impl BackendErrorWrapper {
    pub fn into_inner(self) -> BackendError {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;

    fn mk(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn transport_bucket() {
        for k in [
            ErrorKind::IoError,
            ErrorKind::FatalSendError,
            ErrorKind::FatalReceiveError,
            ErrorKind::ProtocolDesync,
        ] {
            assert_eq!(classify_ferriskey_kind(k), BackendErrorKind::Transport);
        }
    }

    #[test]
    fn cluster_bucket() {
        for k in [
            ErrorKind::Moved,
            ErrorKind::Ask,
            ErrorKind::TryAgain,
            ErrorKind::ClusterDown,
            ErrorKind::CrossSlot,
            ErrorKind::MasterDown,
            ErrorKind::AllConnectionsUnavailable,
            ErrorKind::ConnectionNotFoundForRoute,
        ] {
            assert_eq!(classify_ferriskey_kind(k), BackendErrorKind::Cluster);
        }
    }

    #[test]
    fn auth_bucket() {
        assert_eq!(
            classify_ferriskey_kind(ErrorKind::AuthenticationFailed),
            BackendErrorKind::Auth
        );
        assert_eq!(
            classify_ferriskey_kind(ErrorKind::PermissionDenied),
            BackendErrorKind::Auth
        );
    }

    #[test]
    fn busy_loading_and_script_buckets() {
        assert_eq!(
            classify_ferriskey_kind(ErrorKind::BusyLoadingError),
            BackendErrorKind::BusyLoading
        );
        assert_eq!(
            classify_ferriskey_kind(ErrorKind::NoScriptError),
            BackendErrorKind::ScriptNotLoaded
        );
    }

    #[test]
    fn protocol_bucket() {
        for k in [
            ErrorKind::ResponseError,
            ErrorKind::ParseError,
            ErrorKind::TypeError,
            ErrorKind::InvalidClientConfig,
            ErrorKind::ClientError,
            ErrorKind::ExtensionError,
            ErrorKind::ReadOnly,
        ] {
            assert_eq!(classify_ferriskey_kind(k), BackendErrorKind::Protocol);
        }
    }

    #[test]
    fn from_ferriskey_preserves_message() {
        let err = mk(ErrorKind::IoError);
        let be = backend_error_from_ferriskey(&err);
        assert_eq!(be.kind(), BackendErrorKind::Transport);
        // Display message preserved verbatim.
        assert_eq!(be.message(), err.to_string());
    }

    #[test]
    fn wrapper_conversion_round_trips() {
        let err = mk(ErrorKind::ClusterDown);
        let wrapped: BackendErrorWrapper = err.into();
        assert_eq!(wrapped.into_inner().kind(), BackendErrorKind::Cluster);
    }
}
