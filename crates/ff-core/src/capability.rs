//! RFC-018 Stage A — backend capability discovery surface.
//!
//! Consumers of `Arc<dyn EngineBackend>` (and HTTP clients hitting
//! `ff-server`) historically had no typed way to ask "what can this
//! backend actually do?" before dispatching — they discovered
//! capability gaps empirically, by trying a trait method and catching
//! [`crate::engine_error::EngineError::Unavailable`]. This module
//! adds a first-class discovery primitive: a [`Capability`] enum,
//! [`CapabilityStatus`] shape, [`BackendIdentity`] tuple, and a
//! [`CapabilityMatrix`] container that [`crate::engine_backend::EngineBackend`]
//! exposes via `capabilities_matrix()`.
//!
//! Stage A (this module) is additive: the trait method has a default
//! impl that returns an empty matrix tagged `family = "unknown"`, so
//! out-of-tree backends keep compiling. Concrete in-tree backends
//! (`ValkeyBackend`, `PostgresBackend`) override to report real caps.
//!
//! Stages B + C (follow-up PRs) derive `docs/POSTGRES_PARITY_MATRIX.md`
//! from the runtime matrix and expose `GET /v1/capabilities` on
//! `ff-server`.
//!
//! See `rfcs/RFC-018-backend-capability-discovery.md` for the full
//! design, the four owner-adjudicated open questions, and the
//! Alternatives-considered record.

use std::collections::BTreeMap;

/// Coarse-grained unit of functionality a backend may or may not
/// provide. Granularity is one entry per operator-UI grey-renderable
/// feature — not per trait method. See RFC-018 §9 Q1 for the
/// owner adjudication (`coarse`, recommended by the draft).
///
/// `#[non_exhaustive]`: adding variants in future RFC-018 stages is
/// a minor bump, not a break. Consumers matching on `Capability`
/// must carry a wildcard arm.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Capability {
    // ── Claim + lifecycle ──
    /// Scheduler-routed claim entrypoint
    /// ([`EngineBackend::claim_for_worker`](crate::engine_backend::EngineBackend::claim_for_worker)).
    ClaimForWorker,
    /// Consume a reclaim grant to mint a resumed-kind handle.
    ClaimFromReclaim,
    /// Suspend + resume by buffered-signal count (RFC-013).
    SuspendResumeByCount,

    // ── Cancel ──
    /// Operator-initiated single-execution cancel.
    CancelExecution,
    /// Operator-initiated flow cancel (no wait).
    CancelFlow,
    /// `cancel_flow` with `CancelFlowWait::WaitTimeout(Duration)`
    /// (#298 driver).
    CancelFlowWaitTimeout,
    /// `cancel_flow` with `CancelFlowWait::WaitIndefinite`.
    CancelFlowWaitIndefinite,

    // ── Streams ──
    /// `read_stream` (XRANGE-style bounded read).
    StreamRead,
    /// `tail_stream` (best-effort live tail).
    StreamBestEffortLive,
    /// Durable-summary frames + `read_summary`.
    StreamDurableSummary,

    // ── Signals + waitpoints ──
    /// Deliver an external signal to a pending waitpoint.
    DeliverSignal,
    /// List pending-or-active waitpoints for an execution.
    ListPendingWaitpoints,
    /// Cluster-wide HMAC kid rotation.
    RotateWaitpointHmac,
    /// Idempotent initial HMAC-secret seed (#280).
    SeedWaitpointHmac,

    // ── Budget + quota ──
    /// Worker-handle-path `report_usage`.
    ReportUsage,
    /// Admin-path `report_usage` (no worker handle; #297 driver).
    ReportUsageAdminPath,
    /// Reset a budget's usage counters.
    ResetBudget,

    // ── Ingress ──
    /// Create a flow header.
    CreateFlow,
    /// Create an execution.
    CreateExecution,
    /// Stage a dependency edge.
    StageDependencyEdge,
    /// Apply a staged dependency edge to its downstream child.
    ApplyDependencyToChild,

    // ── Boot + subscriptions ──
    /// Backend honours [`EngineBackend::prepare`](crate::engine_backend::EngineBackend::prepare)
    /// with non-trivial work (e.g. Valkey `FUNCTION LOAD`).
    PreparableBoot,
    /// Subscribe to lease-epoch history for a handle.
    SubscribeLeaseHistory,
    /// Subscribe to completion notifications.
    SubscribeCompletion,
    /// Subscribe to signal-delivery notifications.
    SubscribeSignalDelivery,

    // ── Diagnostics ──
    /// Backend-level reachability probe.
    Ping,
}

/// Per-[`Capability`] support status reported by a concrete backend.
///
/// Consumers distinguish fully-supported from partially-gated
/// ("works only with an extra setup step") and unsupported ("the
/// trait method returns `EngineError::Unavailable` today") via
/// these variants. `Unknown` is the safe default for pre-RFC-018
/// backends that never populate a matrix row.
///
/// `#[non_exhaustive]`: future stages may add e.g. `SupportedSlow`
/// or `Deprecated`; consumers must carry a wildcard arm.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityStatus {
    /// Backend fully supports this capability on every call.
    Supported,
    /// Backend does not support this capability; calling the
    /// corresponding trait method returns `EngineError::Unavailable`.
    Unsupported,
    /// Backend supports this capability only under specific
    /// configuration. The `note` explains the gating constraint in
    /// a human-readable form suitable for UI surfacing.
    Partial {
        /// Human-readable hint describing the gating constraint
        /// (e.g. "requires with_embedded_scheduler").
        note: String,
    },
    /// Backend has not reported a status for this capability.
    /// Consumers should treat as "dispatch and catch" — equivalent
    /// to pre-RFC-018 behaviour.
    Unknown,
}

/// Backend crate version. Kept as a struct (not a semver string) per
/// RFC-018 §9 Q2: consumers can write
/// `if backend.capabilities_matrix().identity.version >= Version::new(0, 10, 0) { .. }`
/// without pulling a semver-parsing dep.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Version {
    /// Major version number.
    pub major: u32,
    /// Minor version number.
    pub minor: u32,
    /// Patch version number.
    pub patch: u32,
}

impl Version {
    /// Const constructor so concrete backends can declare a
    /// `const` [`BackendIdentity`] without a function-call overhead
    /// in `capabilities_matrix()`.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// Minimal-identity triple for a backend. Consumers that only need
/// the family label + version (e.g. for metrics dimensioning) read
/// this rather than the full [`CapabilityMatrix`].
///
/// `#[non_exhaustive]`: future stages may add fields (e.g. a
/// backend-assigned `instance_id` or a `deployment_topology`
/// hint); construct via the public constructor or struct literal on
/// `Clone::clone` of an existing value.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct BackendIdentity {
    /// Stable backend family name. `"valkey"`, `"postgres"`, or a
    /// concrete string set by an out-of-tree backend. `"unknown"`
    /// is the pre-RFC-018 default.
    pub family: &'static str,
    /// Backend crate version at build time.
    pub version: Version,
    /// RFC-017 migration stage this backend reports itself certified
    /// at. One of `"A"`, `"B"`, `"C"`, `"D"`, `"E"`, `"E-shipped"`,
    /// or `"unknown"` for backends that predate the RFC-017 staging.
    pub rfc017_stage: &'static str,
}

impl BackendIdentity {
    /// Direct-field constructor. Prefer this over struct-literal
    /// syntax in consumer code: `#[non_exhaustive]` forbids literal
    /// construction from outside the defining crate.
    pub const fn new(
        family: &'static str,
        version: Version,
        rfc017_stage: &'static str,
    ) -> Self {
        Self {
            family,
            version,
            rfc017_stage,
        }
    }
}

/// Full capability snapshot for a backend: its [`BackendIdentity`]
/// plus a stable-ordered map of [`Capability`] → [`CapabilityStatus`].
///
/// `BTreeMap` (not `HashMap`) so iteration order is deterministic —
/// `ff-server`'s JSON response is byte-stable, cairn's operator UI
/// can render rows in a fixed order, and tests comparing matrices
/// across runs do not race on hash randomization.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct CapabilityMatrix {
    /// Backend identity tuple this matrix was assembled for.
    pub identity: BackendIdentity,
    /// Per-capability status. Absent capabilities are treated as
    /// [`CapabilityStatus::Unknown`] by [`Self::get`].
    pub caps: BTreeMap<Capability, CapabilityStatus>,
}

impl CapabilityMatrix {
    /// Build an empty matrix tagged with the given backend identity.
    /// Backends populate rows via [`Self::set`] before returning the
    /// matrix from `capabilities_matrix()`.
    pub fn new(identity: BackendIdentity) -> Self {
        Self {
            identity,
            caps: BTreeMap::new(),
        }
    }

    /// Record the status for one capability. Returns `&mut self`
    /// so backends can chain setup in a builder-style declaration.
    pub fn set(&mut self, cap: Capability, status: CapabilityStatus) -> &mut Self {
        self.caps.insert(cap, status);
        self
    }

    /// Look up one capability's status. Absent rows return
    /// [`CapabilityStatus::Unknown`] — callers that need to
    /// distinguish "absent" from "explicitly marked unknown" must
    /// consult `self.caps` directly.
    pub fn get(&self, cap: Capability) -> CapabilityStatus {
        self.caps
            .get(&cap)
            .cloned()
            .unwrap_or(CapabilityStatus::Unknown)
    }

    /// Convenience predicate: the capability is
    /// [`CapabilityStatus::Supported`] or [`CapabilityStatus::Partial`].
    /// Both map to "you can call the trait method and it will work
    /// (possibly with a caveat)"; `Unsupported` and `Unknown` both
    /// map to "don't dispatch, or be ready to catch `Unavailable`."
    pub fn supports(&self, cap: Capability) -> bool {
        matches!(
            self.get(cap),
            CapabilityStatus::Supported | CapabilityStatus::Partial { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_new_is_const_and_ordered() {
        const V: Version = Version::new(0, 9, 0);
        assert_eq!(V.major, 0);
        assert_eq!(V.minor, 9);
        assert_eq!(V.patch, 0);
        assert!(Version::new(0, 10, 0) > V);
        assert!(Version::new(0, 9, 1) > V);
        assert!(Version::new(1, 0, 0) > V);
    }

    #[test]
    fn backend_identity_new_populates_fields() {
        let id = BackendIdentity::new("valkey", Version::new(0, 9, 0), "E-shipped");
        assert_eq!(id.family, "valkey");
        assert_eq!(id.version, Version::new(0, 9, 0));
        assert_eq!(id.rfc017_stage, "E-shipped");
    }

    #[test]
    fn matrix_new_is_empty() {
        let m = CapabilityMatrix::new(BackendIdentity::new(
            "unknown",
            Version::new(0, 0, 0),
            "unknown",
        ));
        assert!(m.caps.is_empty());
        // Unset capability resolves to Unknown.
        assert_eq!(m.get(Capability::Ping), CapabilityStatus::Unknown);
        assert!(!m.supports(Capability::Ping));
    }

    #[test]
    fn matrix_set_get_supports() {
        let mut m = CapabilityMatrix::new(BackendIdentity::new(
            "valkey",
            Version::new(0, 9, 0),
            "E-shipped",
        ));
        m.set(Capability::Ping, CapabilityStatus::Supported)
            .set(Capability::CancelExecution, CapabilityStatus::Unsupported)
            .set(
                Capability::ClaimForWorker,
                CapabilityStatus::Partial {
                    note: "requires with_embedded_scheduler".to_string(),
                },
            );

        assert_eq!(m.get(Capability::Ping), CapabilityStatus::Supported);
        assert!(m.supports(Capability::Ping));

        assert_eq!(
            m.get(Capability::CancelExecution),
            CapabilityStatus::Unsupported
        );
        assert!(!m.supports(Capability::CancelExecution));

        // Partial also reports as supported via the predicate.
        assert!(m.supports(Capability::ClaimForWorker));
        match m.get(Capability::ClaimForWorker) {
            CapabilityStatus::Partial { note } => {
                assert!(note.contains("with_embedded_scheduler"));
            }
            other => panic!("expected Partial, got {other:?}"),
        }

        // Chain returns &mut self so repeated .set() calls compose.
        let rows = m.caps.len();
        assert_eq!(rows, 3);
    }

    #[test]
    fn matrix_set_overwrites_existing() {
        let mut m = CapabilityMatrix::new(BackendIdentity::new(
            "valkey",
            Version::new(0, 9, 0),
            "E-shipped",
        ));
        m.set(Capability::Ping, CapabilityStatus::Unsupported);
        m.set(Capability::Ping, CapabilityStatus::Supported);
        assert_eq!(m.get(Capability::Ping), CapabilityStatus::Supported);
        assert_eq!(m.caps.len(), 1);
    }
}
