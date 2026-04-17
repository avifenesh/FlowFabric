use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Macro to define a UUID-backed ID type with standard derives and impls.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// Parse from a UUID string.
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                Ok(Self(Uuid::parse_str(s)?))
            }

            /// Return the raw UUID bytes for partition hashing.
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

/// Macro to define a String-backed ID type.
macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

// ── UUID-backed identity types ──

uuid_id! {
    /// Stable identity for a logical execution. Never changes across retries/reclaims/replays.
    ExecutionId
}

uuid_id! {
    /// Flow container identity.
    FlowId
}

uuid_id! {
    /// Budget definition identity.
    BudgetId
}

uuid_id! {
    /// Quota policy identity.
    QuotaPolicyId
}

uuid_id! {
    /// Lease identity. Created on each lease acquisition.
    LeaseId
}

uuid_id! {
    /// Per-attempt unique identity. Used for audit correlation.
    AttemptId
}

uuid_id! {
    /// Signal identity.
    SignalId
}

uuid_id! {
    /// Waitpoint identity.
    WaitpointId
}

uuid_id! {
    /// Suspension episode identity.
    SuspensionId
}

uuid_id! {
    /// Dependency edge identity.
    EdgeId
}

// ── Numeric types ──

/// Monotonic fencing token. Increments on each new lease issuance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseEpoch(pub u64);

impl LeaseEpoch {
    pub fn new(epoch: u64) -> Self {
        Self(epoch)
    }

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for LeaseEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Zero-based monotonically increasing attempt index within an execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptIndex(pub u32);

impl AttemptIndex {
    pub fn new(index: u32) -> Self {
        Self(index)
    }

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for AttemptIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Millisecond-precision timestamp (Unix epoch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimestampMs(pub i64);

impl TimestampMs {
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_millis() as i64,
        )
    }

    pub fn from_millis(ms: i64) -> Self {
        Self(ms)
    }
}

impl fmt::Display for TimestampMs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── String-backed identity types ──

string_id! {
    /// Logical worker identity (e.g., "worker-gpu-pool-1").
    WorkerId
}

string_id! {
    /// Concrete worker process/runtime instance identity (e.g., container ID).
    WorkerInstanceId
}

string_id! {
    /// Tenant or workspace scope.
    Namespace
}

string_id! {
    /// Submission lane (queue-compatible ingress).
    LaneId
}

string_id! {
    /// Waitpoint key — opaque token used for external signal delivery.
    WaitpointKey
}

/// Waitpoint HMAC token — authenticates signal delivery against the
/// waitpoint's mint-time binding. Format `"kid:40hex"` (see RFC-004
/// §Waitpoint Security). The `kid` prefix identifies which signing
/// key produced the token, enabling zero-downtime rotation.
///
/// **Debug / Display REDACT the hex digest.** This is a bearer credential:
/// anyone who captures the full token can mint signals against the waitpoint
/// until it closes or rotation grace expires. A derive'd `Debug` would leak
/// the digest into every `tracing::debug!(args = ?args)` call; a generic
/// `Display` would leak it into error messages. Both impls print
/// `"<kid>:<REDACTED:len>"` instead. If a test needs the raw value, use
/// `as_str()` explicitly — that makes the leak intentional and searchable.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WaitpointToken(pub String);

impl WaitpointToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Raw token value. Use ONLY at the wire boundary (FCALL ARGV,
    /// HTTP header). Never feed this into a logger.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Split the token into `(kid, digest_len)` for redacted formatting.
    /// Returns `(None, 0)` for malformed inputs so Debug/Display never
    /// accidentally surface partial hex.
    fn parts_for_redaction(&self) -> (Option<&str>, usize) {
        match self.0.find(':') {
            Some(i) if i > 0 && i < self.0.len() - 1 => {
                (Some(&self.0[..i]), self.0.len() - i - 1)
            }
            _ => (None, 0),
        }
    }
}

impl fmt::Debug for WaitpointToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kid, digest_len) = self.parts_for_redaction();
        match kid {
            Some(k) => write!(f, "WaitpointToken({k}:<REDACTED:len={digest_len}>)"),
            None => write!(f, "WaitpointToken(<REDACTED:malformed>)"),
        }
    }
}

impl fmt::Display for WaitpointToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kid, digest_len) = self.parts_for_redaction();
        match kid {
            Some(k) => write!(f, "{k}:<REDACTED:len={digest_len}>"),
            None => write!(f, "<REDACTED:malformed>"),
        }
    }
}

impl From<&str> for WaitpointToken {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for WaitpointToken {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Source of a cancel operation, determining authorization behavior.
/// "operator_override" bypasses lease checks; "lease_holder" requires valid lease.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSource {
    #[default]
    OperatorOverride,
    LeaseHolder,
    FlowCascade,
    SystemTimeout,
    #[serde(untagged)]
    Custom(String),
}

impl CancelSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OperatorOverride => "operator_override",
            Self::LeaseHolder => "lease_holder",
            Self::FlowCascade => "flow_cascade",
            Self::SystemTimeout => "system_timeout",
            Self::Custom(s) => s,
        }
    }
}

impl fmt::Display for CancelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CancelSource {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "operator_override" => Self::OperatorOverride,
            "lease_holder" => Self::LeaseHolder,
            "flow_cascade" => Self::FlowCascade,
            "system_timeout" => Self::SystemTimeout,
            other => Self::Custom(other.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_id_display_roundtrip() {
        let eid = ExecutionId::new();
        let s = eid.to_string();
        let parsed = ExecutionId::parse(&s).unwrap();
        assert_eq!(eid, parsed);
    }

    #[test]
    fn string_id_from_str() {
        let ns = Namespace::new("tenant-1");
        assert_eq!(ns.as_str(), "tenant-1");
        assert_eq!(ns.to_string(), "tenant-1");
    }

    #[test]
    fn lease_epoch_ordering() {
        let e1 = LeaseEpoch::new(1);
        let e2 = e1.next();
        assert!(e2 > e1);
        assert_eq!(e2.0, 2);
    }

    #[test]
    fn attempt_index_ordering() {
        let a0 = AttemptIndex::new(0);
        let a1 = a0.next();
        assert!(a1 > a0);
        assert_eq!(a1.0, 1);
    }

    #[test]
    fn uuid_id_serde_roundtrip() {
        let eid = ExecutionId::new();
        let json = serde_json::to_string(&eid).unwrap();
        let parsed: ExecutionId = serde_json::from_str(&json).unwrap();
        assert_eq!(eid, parsed);
    }

    #[test]
    fn string_id_serde_roundtrip() {
        let lane = LaneId::new("default");
        let json = serde_json::to_string(&lane).unwrap();
        let parsed: LaneId = serde_json::from_str(&json).unwrap();
        assert_eq!(lane, parsed);
    }
}
