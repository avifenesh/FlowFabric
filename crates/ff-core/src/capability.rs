//! RFC-018 Stage A — backend capability discovery surface.
//!
//! Consumers of `Arc<dyn EngineBackend>` (and HTTP clients hitting
//! `ff-server`) historically had no typed way to ask "what can this
//! backend actually do?" before dispatching — they discovered
//! capability gaps empirically, by trying a trait method and catching
//! [`crate::engine_error::EngineError::Unavailable`]. This module
//! adds a first-class discovery primitive: a [`Supports`] flat-bool
//! struct, a [`BackendIdentity`] tuple, and a [`Capabilities`]
//! container that [`crate::engine_backend::EngineBackend`] exposes
//! via `capabilities()`.
//!
//! Stage A (this module) is additive: the trait method has a default
//! impl that returns a `Capabilities` tagged `family = "unknown"` with
//! every `supports.*` bool `false`, so out-of-tree backends keep
//! compiling. Concrete in-tree backends (`ValkeyBackend`,
//! `PostgresBackend`) override to report real caps.
//!
//! **Shape history.** v0.9 shipped a `BTreeMap<Capability,
//! CapabilityStatus>` map; v0.10 reshaped to the flat [`Supports`]
//! struct below per cairn's original #277 ask (flat named-field
//! dot-access, no enum + no map lookup). `Partial`-status nuance
//! (e.g. non-durable cursor on Valkey `subscribe_completion`) now
//! lives in rustdoc on the trait method and
//! `docs/POSTGRES_PARITY_MATRIX.md`; the flat bool answers "is this
//! callable at all."
//!
//! Stages B + C (follow-up PRs) derive `docs/POSTGRES_PARITY_MATRIX.md`
//! from the runtime value and expose `GET /v1/capabilities` on
//! `ff-server`.
//!
//! See `rfcs/RFC-018-backend-capability-discovery.md` for the full
//! design, the four owner-adjudicated open questions, and the
//! Alternatives-considered record.

/// Per-capability boolean support surface. Flat named-field shape so
/// consumers can dot-access (e.g. `caps.supports.cancel_execution`)
/// instead of map lookup. `#[non_exhaustive]` protects future
/// additions from source-breaking consumers; construct via
/// [`Supports::none`] or by returning one from
/// [`crate::engine_backend::EngineBackend::capabilities`].
///
/// # Grouping policy
///
/// One bool per operator-visible HTTP surface; admin-only surfaces
/// with many sibling methods roll up to a single bool (e.g.
/// [`Self::budget_admin`] covers `create_budget` / `report_usage` /
/// `reset_budget` / `get_budget_status` / `report_usage_admin`;
/// [`Self::quota_admin`] covers `create_quota_policy`). Cairn's
/// operator UI grey-renders at the group level; fine-grained
/// pre-dispatch checks still use
/// [`crate::engine_error::EngineError::Unavailable`].
///
/// # Field order
///
/// Per cairn #277 "in the same order as the parity matrix so cairn
/// can consume by copy-paste." Keep in sync with
/// `docs/POSTGRES_PARITY_MATRIX.md`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Supports {
    // ── Execution operator control ──
    /// `cancel_execution`.
    pub cancel_execution: bool,
    /// `change_priority`.
    pub change_priority: bool,
    /// `replay_execution`.
    pub replay_execution: bool,
    /// `revoke_lease`.
    pub revoke_lease: bool,

    // ── Execution reads ──
    /// `read_execution_state`.
    pub read_execution_state: bool,
    /// `read_execution_info`.
    pub read_execution_info: bool,
    /// `get_execution_result`.
    pub get_execution_result: bool,

    // ── Budget + quota (group-level, rolls up siblings) ──
    /// Covers `create_budget` + `report_usage` + `reset_budget` +
    /// `get_budget_status` + `report_usage_admin`.
    pub budget_admin: bool,
    /// Covers `create_quota_policy`.
    pub quota_admin: bool,

    // ── Waitpoint admin ──
    /// `rotate_waitpoint_hmac_secret_all`.
    pub rotate_waitpoint_hmac_secret_all: bool,
    /// `seed_waitpoint_hmac_secret` (#280).
    pub seed_waitpoint_hmac_secret: bool,
    /// `list_pending_waitpoints`.
    pub list_pending_waitpoints: bool,

    // ── Flow control ──
    /// `cancel_flow_header`.
    pub cancel_flow_header: bool,
    /// `cancel_flow` with `CancelFlowWait::WaitTimeout(..)`.
    pub cancel_flow_wait_timeout: bool,
    /// `cancel_flow` with `CancelFlowWait::WaitIndefinite`.
    pub cancel_flow_wait_indefinite: bool,
    /// `ack_cancel_member`.
    pub ack_cancel_member: bool,

    // ── Scheduler ──
    /// `claim_for_worker` (requires a wired scheduler on Valkey;
    /// Postgres-native on Postgres).
    pub claim_for_worker: bool,

    // ── Reclaim (RFC-024) ──
    /// `issue_reclaim_grant` + `reclaim_execution` (rolled up — both
    /// land together on every in-tree backend per RFC-024 §3.6, so
    /// one bool covers the consumer-visible reclaim surface).
    /// `false` on out-of-tree backends via `Supports::none()`; `true`
    /// on Valkey (v0.11.0 per RFC-024 PR-F), Postgres (PR-D), SQLite
    /// (PR-E).
    pub issue_reclaim_grant: bool,

    // ── Boot ──
    /// `prepare` does non-trivial work (e.g. Valkey `FUNCTION LOAD`).
    /// Postgres reports `false` — `prepare` returns `NoOp` there.
    pub prepare: bool,

    // ── Stream subscriptions (RFC-019) ──
    /// `subscribe_lease_history`.
    pub subscribe_lease_history: bool,
    /// `subscribe_completion`. On Valkey this is pubsub-backed
    /// (non-durable cursor, at-most-once over the live subscription
    /// window); Postgres is durable via outbox + cursor. Both report
    /// `true`; see the trait method rustdoc for the non-durable-cursor
    /// caveat and `docs/POSTGRES_PARITY_MATRIX.md` for the per-backend
    /// semantic.
    pub subscribe_completion: bool,
    /// `subscribe_signal_delivery`.
    pub subscribe_signal_delivery: bool,
    /// `subscribe_instance_tags`. Deferred per #311 (cairn's `instance_tag_backfill`
    /// pattern is served by `list_executions` + `ScannerFilter::with_instance_tag(..)`);
    /// reported `false` on both backends today.
    pub subscribe_instance_tags: bool,

    // ── Streaming (RFC-015) ──
    /// `read_summary` + durable-summary frames.
    pub stream_durable_summary: bool,
    /// `tail_stream` (best-effort live tail).
    pub stream_best_effort_live: bool,
    // Add new fields here, preserving parity-matrix order.
}

impl Supports {
    /// Construct a `Supports` with every field `false`. Useful as a
    /// starting point when assembling a backend-specific capability
    /// snapshot. Consumers should never see this directly —
    /// `capabilities()` on a real backend always returns a populated
    /// instance.
    pub const fn none() -> Self {
        Self {
            cancel_execution: false,
            change_priority: false,
            replay_execution: false,
            revoke_lease: false,
            read_execution_state: false,
            read_execution_info: false,
            get_execution_result: false,
            budget_admin: false,
            quota_admin: false,
            rotate_waitpoint_hmac_secret_all: false,
            seed_waitpoint_hmac_secret: false,
            list_pending_waitpoints: false,
            cancel_flow_header: false,
            cancel_flow_wait_timeout: false,
            cancel_flow_wait_indefinite: false,
            ack_cancel_member: false,
            claim_for_worker: false,
            issue_reclaim_grant: false,
            prepare: false,
            subscribe_lease_history: false,
            subscribe_completion: false,
            subscribe_signal_delivery: false,
            subscribe_instance_tags: false,
            stream_durable_summary: false,
            stream_best_effort_live: false,
        }
    }
}

/// Backend crate version. Kept as a struct (not a semver string) per
/// RFC-018 §9 Q2: consumers can write
/// `if backend.capabilities().identity.version >= Version::new(0, 10, 0) { .. }`
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
    /// Const constructor so concrete backends can declare a `const`
    /// [`BackendIdentity`] without a function-call overhead in
    /// `capabilities()`.
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
/// this rather than the full [`Capabilities`].
///
/// `#[non_exhaustive]`: future stages may add fields (e.g. a
/// backend-assigned `instance_id` or a `deployment_topology`
/// hint); construct via [`Self::new`] or by returning one from
/// [`crate::engine_backend::EngineBackend::capabilities`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
/// plus a flat [`Supports`] surface of per-method bools.
///
/// Consumers typically read `caps.supports.<field>` to gate a UI
/// surface or choose between two code paths before dispatching; the
/// [`Self::identity`] side exists for metrics dimensioning + UI
/// labelling.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// Backend identity tuple this snapshot was assembled for.
    pub identity: BackendIdentity,
    /// Per-capability support bools.
    pub supports: Supports,
}

impl Capabilities {
    /// Construct a `Capabilities` value from an identity + a populated
    /// [`Supports`]. Backends typically build one in `capabilities()`
    /// without going through a constructor; this exists for
    /// out-of-tree backends that prefer the explicit call.
    pub const fn new(identity: BackendIdentity, supports: Supports) -> Self {
        Self { identity, supports }
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
    fn supports_none_is_all_false() {
        let s = Supports::none();
        // Spot-check a handful across the grouping policy; `Default`
        // covers the exhaustive guarantee via `assert_eq!` below.
        assert!(!s.cancel_execution);
        assert!(!s.budget_admin);
        assert!(!s.quota_admin);
        assert!(!s.subscribe_instance_tags);
        assert!(!s.stream_durable_summary);
        // `Default::default()` must match `none()` so consumers that
        // lean on `..Default::default()` get the same zero state.
        assert_eq!(s, Supports::default());
    }

    #[test]
    fn capabilities_new_wires_identity_and_supports() {
        let mut s = Supports::none();
        s.cancel_execution = true;
        let caps = Capabilities::new(
            BackendIdentity::new("valkey", Version::new(0, 10, 0), "E-shipped"),
            s,
        );
        assert_eq!(caps.identity.family, "valkey");
        assert!(caps.supports.cancel_execution);
        assert!(!caps.supports.change_priority);
    }
}
