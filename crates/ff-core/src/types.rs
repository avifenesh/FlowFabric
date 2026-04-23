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

// ── ExecutionId — bespoke impl (RFC-011 §2.3) ──
//
// Not uuid_id!-generated because the hash-tagged `{fp:N}:<uuid>` shape
// requires a flow/lane context at construction time (co-locating exec keys
// with their parent flow's Valkey slot). Removes new()/Default/from_uuid
// variants that would produce a bare UUID with no hash-tag.

/// Stable identity for a logical execution. Never changes across
/// retries/reclaims/replays.
///
/// String-backed shape `{fp:N}:<uuid>` where `fp:N` is the Valkey
/// hash-tag for the flow partition this execution co-locates with, and
/// `<uuid>` is an opaque 128-bit identifier for intra-partition
/// uniqueness. [`ExecutionId::for_flow`] / [`ExecutionId::solo`] mint a
/// UUIDv4; [`ExecutionId::deterministic_for_flow`] /
/// [`ExecutionId::deterministic_solo`] accept any caller-supplied
/// `Uuid` without version or shape strictness — the caller owns the
/// derivation scheme.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ExecutionId(String);

/// Error returned when parsing a malformed [`ExecutionId`] string.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExecutionIdParseError {
    #[error("execution_id is missing the `{{fp:N}}:` hash-tag prefix: {0}")]
    MissingTag(String),
    #[error("execution_id partition index is not a valid u16: {0}")]
    InvalidPartitionIndex(String),
    #[error("execution_id UUID suffix is not a valid UUID: {0}")]
    InvalidUuid(String),
}

impl ExecutionId {
    /// Mint an execution id co-located with the given flow's partition.
    /// Use when the flow is known at creation time (the common case).
    ///
    /// Partition index = `crc16(flow_id.bytes) % num_flow_partitions`,
    /// matching [`crate::partition::flow_partition`].
    pub fn for_flow(flow_id: &FlowId, config: &crate::partition::PartitionConfig) -> Self {
        let partition = crate::partition::flow_partition(flow_id, config).index;
        Self::with_partition(partition)
    }

    /// Mint an execution id co-located with the given lane's solo shard.
    /// Use when the exec has no parent flow (standalone `create_execution`).
    ///
    /// Partition index = `crc16(lane_id.as_str().bytes) % num_flow_partitions`
    /// via the default [`crate::partition::SoloPartitioner`].
    pub fn solo(lane_id: &LaneId, config: &crate::partition::PartitionConfig) -> Self {
        let partition = crate::partition::solo_partition(lane_id, config).index;
        Self::with_partition(partition)
    }

    fn with_partition(partition: u16) -> Self {
        let uuid = Uuid::new_v4();
        Self(format!("{{fp:{partition}}}:{uuid}"))
    }

    fn with_partition_and_uuid(partition: u16, uuid: Uuid) -> Self {
        Self(format!("{{fp:{partition}}}:{uuid}"))
    }

    /// Deterministic mint: same `(flow_id, uuid)` inputs always produce the
    /// same `ExecutionId`. Intended for consumers that derive a stable UUID
    /// externally (e.g. `Uuid::new_v5` from application-scoped keys) and
    /// need idempotent execution identity on retry without coordinating
    /// through FlowFabric.
    ///
    /// Any `Uuid` value is accepted — no version or shape strictness. The
    /// caller owns the derivation scheme; FlowFabric treats the UUID as an
    /// opaque 128-bit identifier.
    ///
    /// # Partition-count stability
    ///
    /// The returned id's hash-tag embeds
    /// `crc16(flow_id.bytes) % num_flow_partitions`. **Determinism holds
    /// only while `num_flow_partitions` is stable** across the caller's
    /// usage window. Resizing the deployment's partition count invalidates
    /// previously-minted ids: the same inputs will produce a different
    /// hash-tag, and lookups against the old id will miss. Consumers relying
    /// on cross-run determinism should pin `num_flow_partitions` or embed a
    /// namespace-version in their UUID derivation so a resize surfaces as a
    /// namespace rollover rather than silent drift.
    pub fn deterministic_for_flow(
        flow_id: &FlowId,
        config: &crate::partition::PartitionConfig,
        uuid: Uuid,
    ) -> Self {
        let partition = crate::partition::flow_partition(flow_id, config).index;
        Self::with_partition_and_uuid(partition, uuid)
    }

    /// Deterministic mint for solo (no-parent-flow) executions. Same
    /// `(lane_id, uuid)` always yields the same `ExecutionId`. Mirrors
    /// [`ExecutionId::solo`] but with a caller-supplied UUID.
    ///
    /// Partition routing uses the default
    /// [`crate::partition::Crc16SoloPartitioner`]. Deployments that
    /// installed a custom [`crate::partition::SoloPartitioner`] must mint
    /// via their own path — this constructor hard-codes the default and
    /// will diverge from a custom partitioner's routing.
    ///
    /// # Partition-count stability
    ///
    /// Same stability contract as [`ExecutionId::deterministic_for_flow`]:
    /// determinism holds only while `num_flow_partitions` is stable.
    pub fn deterministic_solo(
        lane_id: &LaneId,
        config: &crate::partition::PartitionConfig,
        uuid: Uuid,
    ) -> Self {
        let partition = crate::partition::solo_partition(lane_id, config).index;
        Self::with_partition_and_uuid(partition, uuid)
    }

    /// Parse a hash-tagged execution-id string.
    ///
    /// Rejects:
    ///
    /// - strings missing the `{fp:N}:` prefix (returns [`ExecutionIdParseError::MissingTag`]);
    /// - non-integer partition index, or one that does not fit in `u16` (returns [`ExecutionIdParseError::InvalidPartitionIndex`]);
    /// - malformed UUID suffix (returns [`ExecutionIdParseError::InvalidUuid`]).
    ///
    /// **Parse does NOT check `N < num_flow_partitions`** — the function takes only `&str` and has no [`crate::partition::PartitionConfig`] handle. Range validation against the live deployment's partition count is the caller's responsibility via [`ExecutionId::partition`] + config comparison.
    ///
    /// See RFC-011 §2.3.1 for the architectural rationale: exec ids are durable identifiers that legitimately cross deployment boundaries with different configs, so parse-time config-coupling would mis-reject otherwise-valid historical ids. The natural validation point is at ingress boundaries (e.g. `ff-server`'s request handlers, scheduler claim-grant receipt) against the current deployment's config.
    ///
    /// Callers reading [`ExecutionId`] strings from Lua FCALL results use this entry point.
    pub fn parse(s: &str) -> Result<Self, ExecutionIdParseError> {
        // Expected: "{fp:<N>}:<uuid>"
        let rest = s
            .strip_prefix("{fp:")
            .ok_or_else(|| ExecutionIdParseError::MissingTag(s.to_owned()))?;
        let close = rest
            .find("}:")
            .ok_or_else(|| ExecutionIdParseError::MissingTag(s.to_owned()))?;
        let partition_str = &rest[..close];
        let uuid_str = &rest[close + 2..];
        partition_str
            .parse::<u16>()
            .map_err(|_| ExecutionIdParseError::InvalidPartitionIndex(partition_str.to_owned()))?;
        Uuid::parse_str(uuid_str)
            .map_err(|_| ExecutionIdParseError::InvalidUuid(uuid_str.to_owned()))?;
        Ok(Self(s.to_owned()))
    }

    /// Decode the partition index from the hash-tag prefix. Infallible on a
    /// validated `ExecutionId` (construction paths guarantee a well-formed tag).
    pub fn partition(&self) -> u16 {
        // Safe: the only ways to construct an ExecutionId are for_flow/solo/parse,
        // all of which enforce the `{fp:N}:<uuid>` shape.
        let rest = &self.0["{fp:".len()..];
        let close = rest.find("}:").expect("invariant: valid ExecutionId");
        rest[..close]
            .parse::<u16>()
            .expect("invariant: valid partition index")
    }

    /// Raw string form; always `{fp:N}:<uuid>`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for ExecutionId {
    /// Deserialises from a JSON string; validates via [`ExecutionId::parse`]
    /// so malformed wire payloads (e.g. legacy bare UUIDs) fail loudly at the
    /// parse boundary rather than routing to the wrong partition downstream.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
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

// ── Lease fence triple ──

/// RFC #58.5 — the fence triple `(lease_id, lease_epoch, attempt_id)` that
/// a lease-bound FCALL passes to assert "I am the rightful owner of the
/// current lease and I expect the server's view to match".
///
/// When present (non-empty), the Lua side runs the stale-lease check
/// (`validate_lease_and_mark_expired`) and rejects mismatches with
/// `stale_lease`. When absent (`None`), the Lua side either:
///   * Accepts the call as an operator override (terminal ops only —
///     `ff_complete_execution`, `ff_fail_execution`, `ff_delay_execution`,
///     `ff_move_to_waiting_children` — gated on a separate `source ==
///     "operator_override"` ARGV), or
///   * Hard-rejects with `fence_required` (`ff_renew_lease`,
///     `ff_suspend_execution` — no override path).
///
/// Every SDK worker-driven call site MUST pass `Some(fence)`; unfenced
/// callers are operator tooling, janitors, and read-model paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFence {
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_id: AttemptId,
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

// ── LaneId — bespoke impl (RFC-011 §9.15) ──
//
// Specialised out of the string_id! macro so ingress (HTTP query params,
// request-body deserialisation) can reject malformed lane strings at the
// system boundary instead of silently hashing them via the SoloPartitioner.

/// Submission lane (queue-compatible ingress).
///
/// `PartialOrd` / `Ord` derive lexicographic order over the inner
/// string — used by
/// [`crate::engine_backend::EngineBackend::list_lanes`] to sort the
/// `ff:idx:lanes` registry into a stable page order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct LaneId(pub String);

/// Max byte length for a LaneId. Matches the common runtime-label ceiling
/// (e.g. Kubernetes label value 63 bytes; we go 64 for round-number reasons).
pub const LANE_ID_MAX_BYTES: usize = 64;

/// Error returned when a LaneId fails validation.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneIdError {
    #[error("lane_id is empty")]
    Empty,
    #[error("lane_id exceeds {LANE_ID_MAX_BYTES} bytes (got {0})")]
    TooLong(usize),
    #[error("lane_id contains non-ASCII-printable byte at index {0}")]
    NonPrintable(usize),
}

impl LaneId {
    /// Infallible constructor. Accepts any `Into<String>` and does NOT validate.
    /// Use for hardcoded lane names ("default", test fixtures). For runtime-
    /// bounded input (HTTP, env, config file), use [`LaneId::try_new`].
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Fallible constructor. Validates:
    ///  - non-empty
    ///  - <= [`LANE_ID_MAX_BYTES`] bytes
    ///  - every byte is ASCII-printable (`0x20..=0x7e`)
    ///
    /// Use at system boundaries where the lane string could be anything.
    pub fn try_new(value: impl Into<String>) -> Result<Self, LaneIdError> {
        let s = value.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    fn validate(s: &str) -> Result<(), LaneIdError> {
        if s.is_empty() {
            return Err(LaneIdError::Empty);
        }
        if s.len() > LANE_ID_MAX_BYTES {
            return Err(LaneIdError::TooLong(s.len()));
        }
        if let Some((idx, _)) = s
            .bytes()
            .enumerate()
            .find(|(_, b)| !(0x20..=0x7e).contains(b))
        {
            return Err(LaneIdError::NonPrintable(idx));
        }
        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for LaneId {
    /// Deserialises from a JSON string; validates via [`LaneId::try_new`]
    /// so malformed ingress lanes fail at the parse boundary instead of
    /// downstream in the partitioner.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl From<&str> for LaneId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for LaneId {
    fn from(s: String) -> Self {
        Self(s)
    }
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
            Some(i) if i > 0 && i < self.0.len() - 1 => (Some(&self.0[..i]), self.0.len() - i - 1),
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
    fn execution_id_display_roundtrip() {
        // Post-RFC-011: ExecutionId has no ::new(); round-trip via for_flow.
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let s = eid.to_string();
        let parsed = ExecutionId::parse(&s).unwrap();
        assert_eq!(eid, parsed);
    }

    #[test]
    fn uuid_id_display_roundtrip() {
        // Non-ExecutionId uuid_id! types still round-trip via ::new().
        let fid = FlowId::new();
        let s = fid.to_string();
        let parsed = FlowId::parse(&s).unwrap();
        assert_eq!(fid, parsed);
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
    fn execution_id_serde_roundtrip() {
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let json = serde_json::to_string(&eid).unwrap();
        let parsed: ExecutionId = serde_json::from_str(&json).unwrap();
        assert_eq!(eid, parsed);
    }

    #[test]
    fn uuid_id_serde_roundtrip() {
        let fid = FlowId::new();
        let json = serde_json::to_string(&fid).unwrap();
        let parsed: FlowId = serde_json::from_str(&json).unwrap();
        assert_eq!(fid, parsed);
    }

    #[test]
    fn string_id_serde_roundtrip() {
        let lane = LaneId::new("default");
        let json = serde_json::to_string(&lane).unwrap();
        let parsed: LaneId = serde_json::from_str(&json).unwrap();
        assert_eq!(lane, parsed);
    }

    // ── Deterministic mint (RFC-011 — cairn-fabric idempotency) ──

    #[test]
    fn deterministic_for_flow_is_stable() {
        let config = crate::partition::PartitionConfig::default();
        let fid = FlowId::new();
        let u = Uuid::new_v4();
        let a = ExecutionId::deterministic_for_flow(&fid, &config, u);
        let b = ExecutionId::deterministic_for_flow(&fid, &config, u);
        assert_eq!(a, b);
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn deterministic_for_flow_partition_matches_for_flow() {
        let config = crate::partition::PartitionConfig::default();
        let fid = FlowId::new();
        let u = Uuid::new_v4();
        let det = ExecutionId::deterministic_for_flow(&fid, &config, u);
        let rnd = ExecutionId::for_flow(&fid, &config);
        assert_eq!(det.partition(), rnd.partition());
    }

    #[test]
    fn deterministic_for_flow_uuid_embedded_verbatim() {
        let config = crate::partition::PartitionConfig::default();
        let fid = FlowId::new();
        let u = Uuid::new_v4();
        let eid = ExecutionId::deterministic_for_flow(&fid, &config, u);
        assert!(eid.as_str().ends_with(&u.to_string()));
    }

    #[test]
    fn deterministic_for_flow_accepts_any_uuid_version() {
        // No strictness — v5 (cairn's case), v4, and nil all accepted.
        let config = crate::partition::PartitionConfig::default();
        let fid = FlowId::new();
        // Include nil, v4, and a synthetic from_bytes (stand-in for v5/v7
        // which aren't enabled as uuid features in this crate).
        for u in [Uuid::nil(), Uuid::new_v4(), Uuid::from_bytes([0xab; 16])] {
            let eid = ExecutionId::deterministic_for_flow(&fid, &config, u);
            assert!(ExecutionId::parse(eid.as_str()).is_ok());
        }
    }

    #[test]
    fn deterministic_for_flow_resize_changes_partition() {
        // Documents the stability contract: different num_flow_partitions
        // generally yields a different partition (i.e. a different id).
        let small = crate::partition::PartitionConfig {
            num_flow_partitions: 4,
            num_budget_partitions: 32,
            num_quota_partitions: 32,
        };
        let big = crate::partition::PartitionConfig {
            num_flow_partitions: 256,
            num_budget_partitions: 32,
            num_quota_partitions: 32,
        };
        // Deterministic search across a fixed byte-pattern set of flow ids
        // so the test outcome does not depend on RNG. 4 divides 256 so the
        // same fid *could* collide across configs; across 256 varied fids
        // the hash-tags cannot be uniformly equal.
        let u = Uuid::from_bytes([0x5a; 16]);
        let any_diff = (0u8..=u8::MAX).any(|i| {
            let mut bytes = [0u8; 16];
            bytes[0] = i;
            bytes[15] = i.wrapping_mul(31).wrapping_add(17);
            let fid = FlowId::from_uuid(Uuid::from_bytes(bytes));
            let a = ExecutionId::deterministic_for_flow(&fid, &small, u);
            let b = ExecutionId::deterministic_for_flow(&fid, &big, u);
            a.partition() != b.partition()
        });
        assert!(any_diff, "partition resize should change at least one id");
    }

    #[test]
    fn deterministic_solo_is_stable() {
        let config = crate::partition::PartitionConfig::default();
        let lane = LaneId::new("default");
        let u = Uuid::new_v4();
        let a = ExecutionId::deterministic_solo(&lane, &config, u);
        let b = ExecutionId::deterministic_solo(&lane, &config, u);
        assert_eq!(a, b);
    }

    #[test]
    fn deterministic_solo_partition_matches_solo() {
        let config = crate::partition::PartitionConfig::default();
        let lane = LaneId::new("default");
        let u = Uuid::new_v4();
        let det = ExecutionId::deterministic_solo(&lane, &config, u);
        let rnd = ExecutionId::solo(&lane, &config);
        assert_eq!(det.partition(), rnd.partition());
    }

    #[test]
    fn deterministic_mint_parse_roundtrip() {
        let config = crate::partition::PartitionConfig::default();
        let fid = FlowId::new();
        let u = Uuid::new_v4();
        let eid = ExecutionId::deterministic_for_flow(&fid, &config, u);
        let parsed = ExecutionId::parse(eid.as_str()).unwrap();
        assert_eq!(eid, parsed);
    }

    // ── LaneId validation (RFC-011 §9.15) ──

    #[test]
    fn lane_id_try_new_accepts_valid() {
        assert!(LaneId::try_new("default").is_ok());
        assert!(LaneId::try_new("worker-pool-1").is_ok());
        assert!(LaneId::try_new("a").is_ok());
        // Exactly max length
        let max = "a".repeat(LANE_ID_MAX_BYTES);
        assert!(LaneId::try_new(max).is_ok());
    }

    #[test]
    fn lane_id_try_new_rejects_empty() {
        assert_eq!(LaneId::try_new(""), Err(LaneIdError::Empty));
    }

    #[test]
    fn lane_id_try_new_rejects_too_long() {
        let over = "a".repeat(LANE_ID_MAX_BYTES + 1);
        match LaneId::try_new(over) {
            Err(LaneIdError::TooLong(n)) => assert_eq!(n, LANE_ID_MAX_BYTES + 1),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn lane_id_try_new_rejects_non_ascii_printable() {
        // Control character
        match LaneId::try_new("bad\nlane") {
            Err(LaneIdError::NonPrintable(3)) => {}
            other => panic!("expected NonPrintable(3), got {other:?}"),
        }
        // Non-ASCII (multi-byte UTF-8)
        match LaneId::try_new("lane\u{00e9}") {
            Err(LaneIdError::NonPrintable(_)) => {}
            other => panic!("expected NonPrintable, got {other:?}"),
        }
        // NUL byte
        match LaneId::try_new("lane\0x") {
            Err(LaneIdError::NonPrintable(4)) => {}
            other => panic!("expected NonPrintable(4), got {other:?}"),
        }
        // Tab (below printable range)
        match LaneId::try_new("\tlane") {
            Err(LaneIdError::NonPrintable(0)) => {}
            other => panic!("expected NonPrintable(0), got {other:?}"),
        }
        // DEL (0x7f, just above printable range)
        match LaneId::try_new("la\u{007f}ne") {
            Err(LaneIdError::NonPrintable(2)) => {}
            other => panic!("expected NonPrintable(2), got {other:?}"),
        }
    }

    #[test]
    fn lane_id_new_is_infallible_for_hardcoded_names() {
        // Hardcoded internal names use ::new (no validation).
        // This test documents the contract that ::new does NOT validate.
        let _ = LaneId::new("default");
        let _ = LaneId::new("test-fixture");
    }

    #[test]
    fn lane_id_deserialize_rejects_malformed() {
        // Empty string via JSON
        let e: Result<LaneId, _> = serde_json::from_str(r#""""#);
        assert!(e.is_err(), "empty string must fail Deserialize");
        // Over-length via JSON
        let over = format!(r#""{}""#, "a".repeat(LANE_ID_MAX_BYTES + 1));
        let e: Result<LaneId, _> = serde_json::from_str(&over);
        assert!(e.is_err(), "over-length must fail Deserialize");
        // Control char via JSON
        let e: Result<LaneId, _> = serde_json::from_str(r#""bad\nlane""#);
        assert!(e.is_err(), "newline must fail Deserialize");
    }

    #[test]
    fn lane_id_deserialize_accepts_valid() {
        let parsed: LaneId = serde_json::from_str(r#""default""#).unwrap();
        assert_eq!(parsed, LaneId::new("default"));
    }
}
