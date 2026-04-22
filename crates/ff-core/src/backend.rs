//! Backend-trait supporting types (RFC-012 Stage 0).
//!
//! This module carries the public types referenced by the `EngineBackend`
//! trait signatures in RFC-012 §3.3. The trait itself lands in Stage 1
//! (issue #89 follow-up); Stage 0 is strictly type-plumbing and the
//! `ResumeSignal` crate move.
//!
//! Public structs/enums whose fields or variants are expected to grow
//! are marked `#[non_exhaustive]` per project convention — consumers
//! must write `_`-terminated matches and use the provided constructors
//! rather than struct literals. Exceptions:
//!
//! * Opaque single-field wrapper newtypes ([`HandleOpaque`],
//!   [`WaitpointHmac`]) hide their inner field and need no non-exhaustive
//!   annotation — the wrapped value is unreachable from outside.
//! * [`ResumeSignal`] is intentionally NOT `#[non_exhaustive]` so the
//!   ff-sdk crate-move (Stage 0) preserves struct-literal compatibility
//!   at its existing call site.
//!
//! See `rfcs/RFC-012-engine-backend-trait.md` §3.3.0 for the authoritative
//! type inventory and §4.1-§4.2 for the `Handle` / `EngineError` shapes.

use crate::contracts::ReclaimGrant;
use crate::types::{TimestampMs, WaitpointToken};
use std::collections::BTreeMap;
use std::time::Duration;

// ── §4.1 Handle trio ────────────────────────────────────────────────────

/// Backend-tag discriminator embedded in every [`Handle`] so ops can
/// dispatch to the correct backend implementation at runtime.
///
/// `#[non_exhaustive]`: new backend variants land additively as impls
/// come online (e.g. `Postgres` in Stage 2, hypothetical third backends
/// later).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendTag {
    /// The Valkey FCALL-backed implementation.
    Valkey,
}

/// Lifecycle kind carried inside a [`Handle`]. Backends validate `kind`
/// on entry to each op and return `EngineError::State` on mismatch.
///
/// Replaces round-1's compile-time `Handle` / `ResumeHandle` /
/// `SuspendToken` type split (RFC-012 §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HandleKind {
    /// Fresh claim — returned by `claim` / `claim_from_grant`.
    Fresh,
    /// Resumed from reclaim — returned by `claim_from_reclaim`.
    Resumed,
    /// Suspended — returned by `suspend`. Terminal for the lease;
    /// resumption mints a new Handle via `claim_from_reclaim`.
    Suspended,
}

/// Backend-private opaque payload carried inside a [`Handle`].
///
/// Encodes backend-specific state (on Valkey: exec id, attempt id,
/// lease id, lease epoch, capability binding, partition). Consumers do
/// not construct or inspect the bytes — they are produced by the
/// backend on claim/resume and consumed by the backend on each op.
///
/// `Box<[u8]>` chosen over `bytes::Bytes` (RFC-012 §7.17) to avoid a
/// public-type transitive dep on the `bytes` crate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HandleOpaque(Box<[u8]>);

impl HandleOpaque {
    /// Construct from backend-owned bytes. Only backend impls call this.
    pub fn new(bytes: Box<[u8]>) -> Self {
        Self(bytes)
    }

    /// Borrow the underlying bytes (backend-internal use).
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Opaque attempt cookie held by the worker for the duration of an
/// attempt. Produced by `claim` / `claim_from_reclaim` / `suspend`;
/// borrowed by every op (renew, progress, append_frame, complete, fail,
/// cancel, suspend, delay, wait_children, observe_signals, report_usage).
///
/// See RFC-012 §4.1 for the round-4 design — terminal ops borrow rather
/// than consume so callers can retry after a transport error.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Handle {
    pub backend: BackendTag,
    pub kind: HandleKind,
    pub opaque: HandleOpaque,
}

impl Handle {
    /// Construct a new Handle. Called by backend impls only; consumer
    /// code receives Handles from `claim` / `suspend` / `claim_from_reclaim`.
    pub fn new(backend: BackendTag, kind: HandleKind, opaque: HandleOpaque) -> Self {
        Self {
            backend,
            kind,
            opaque,
        }
    }
}

// ── §3.3.0 Claim / lifecycle supporting types ──────────────────────────

/// Worker capability set — the tokens the worker advertises to the
/// scheduler and to `claim`. Today stored as `Vec<String>` on
/// `WorkerConfig`; promoted to a named newtype so the trait signatures
/// can talk about capabilities without committing to a concrete
/// container shape.
///
/// Bitfield vs stringly-typed is §7.2 open question; Stage 0 keeps the
/// round-2 lean (newtype over `Vec<String>`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CapabilitySet {
    pub tokens: Vec<String>,
}

impl CapabilitySet {
    /// Build from any iterable of string-like capability tokens.
    pub fn new<I, S>(tokens: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            tokens: tokens.into_iter().map(Into::into).collect(),
        }
    }

    /// True iff the set contains no tokens.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Policy hints for `claim`. Minimal at Stage 0 per RFC-012 §3.3.0
/// ("Bikeshed-prone; keep minimal at Stage 0"). Future fields (retry
/// count, fairness hints) land additively.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ClaimPolicy {
    /// Maximum blocking wait. `None` means backend-default (today:
    /// non-blocking / immediate return).
    pub max_wait: Option<Duration>,
}

impl ClaimPolicy {
    /// Zero-timeout claim (non-blocking). Matches today's SDK default.
    pub fn immediate() -> Self {
        Self { max_wait: None }
    }

    /// Claim with an explicit blocking bound.
    pub fn with_max_wait(max_wait: Duration) -> Self {
        Self {
            max_wait: Some(max_wait),
        }
    }
}

/// Frame classification for `append_frame`. Mirrors the Lua-side
/// `ff_append_frame` `frame_type` ARGV.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameKind {
    /// Operator-visible progress output / log line.
    Stdout,
    /// Operator-visible error / warning output.
    Stderr,
    /// Structured event (JSON payload).
    Event,
    /// Binary / opaque payload.
    Blob,
}

/// Single stream frame appended via `append_frame` (RFC-012 §3.3.0).
/// Today's FCALL takes the byte payload + frame_type + optional seq as
/// discrete ARGV; Stage 0 collects them into a named type for trait
/// signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub kind: FrameKind,
    /// Optional monotonic sequence. Set by the caller when the stream
    /// protocol is sequence-bound; `None` lets the backend assign.
    pub seq: Option<u64>,
}

impl Frame {
    /// Construct a frame. `seq` defaults to `None` (backend-assigned);
    /// callers that need an explicit sequence use
    /// [`Frame::with_seq`].
    pub fn new(bytes: Vec<u8>, kind: FrameKind) -> Self {
        Self {
            bytes,
            kind,
            seq: None,
        }
    }

    /// Construct a frame with an explicit monotonic sequence.
    pub fn with_seq(bytes: Vec<u8>, kind: FrameKind, seq: u64) -> Self {
        Self {
            bytes,
            kind,
            seq: Some(seq),
        }
    }
}

// ── §3.3.0 Suspend / waitpoint types ────────────────────────────────────

/// Waitpoint matcher mode (mirrors today's suspend/close matcher kinds
/// — signal name, correlation id, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WaitpointKind {
    /// Match by signal name.
    SignalName,
    /// Match by correlation id.
    CorrelationId,
    /// Generic external-signal waitpoint (external delivery).
    External,
}

/// HMAC token that binds a waitpoint to its mint-time identity. Wire
/// shape `kid:40hex`.
///
/// Wraps [`crate::types::WaitpointToken`] so bearer-credential Debug /
/// Display redaction (`WaitpointToken("kid1:<REDACTED:len=40>")`) flows
/// through automatically — no derived formatter can leak the raw
/// digest when a [`WaitpointSpec`] is debug-printed via
/// `tracing::debug!(spec=?spec)`.
///
/// Newtype-wrapping (vs. a `pub use` alias) keeps trait signatures
/// naming the waitpoint-bound HMAC role explicitly.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WaitpointHmac(WaitpointToken);

impl WaitpointHmac {
    pub fn new(token: impl Into<String>) -> Self {
        Self(WaitpointToken::from(token.into()))
    }

    /// Borrow the wrapped token. The wrapped type's `Debug`/`Display`
    /// redact — call sites that need the raw digest must explicitly
    /// call [`WaitpointToken::as_str`].
    pub fn token(&self) -> &WaitpointToken {
        &self.0
    }

    /// Borrow the raw `kid:40hex` string. Prefer [`Self::token`] for
    /// non-redacted call sites; this method exists only for transport
    /// / FCALL ARGV construction where the raw wire bytes are required.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

// Forward Debug / Display to the wrapped WaitpointToken so the
// redaction guarantees on that type extend here. Derived Debug would
// expose the raw string field and defeat the wrap.
impl std::fmt::Debug for WaitpointHmac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WaitpointHmac({:?})", self.0)
    }
}

/// One waitpoint inside a suspend request. `suspend` takes a
/// `Vec<WaitpointSpec>`; the resume condition (`any` / `all`) lives on
/// the enclosing suspend args in the Phase-1 contract.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitpointSpec {
    pub kind: WaitpointKind,
    pub matcher: Vec<u8>,
    pub hmac_token: WaitpointHmac,
}

impl WaitpointSpec {
    pub fn new(kind: WaitpointKind, matcher: Vec<u8>, hmac_token: WaitpointHmac) -> Self {
        Self {
            kind,
            matcher,
            hmac_token,
        }
    }
}

// ── §3.3.0 Failure classification types ─────────────────────────────────

/// Human-readable failure description + optional structured detail.
/// Replaces today's ad-hoc string arg to `fail`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FailureReason {
    pub message: String,
    pub detail: Option<Vec<u8>>,
}

impl FailureReason {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(message: impl Into<String>, detail: Vec<u8>) -> Self {
        Self {
            message: message.into(),
            detail: Some(detail),
        }
    }
}

/// Failure classification — determines retry disposition on the Lua
/// side. Mirrors the Lua-side classification codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureClass {
    /// Retryable transient error.
    Transient,
    /// Permanent error — no retries.
    Permanent,
    /// Crash / process death inferred from lease expiry.
    InfraCrash,
    /// Timeout at the attempt or operation level.
    Timeout,
    /// Cooperative cancellation by operator or cancel_flow.
    Cancelled,
}

// ── §3.3.0 Usage / budget types ─────────────────────────────────────────

/// Usage report for `report_usage`. Mirrors today's
/// `ff_report_usage_and_check` ARGV: token-counts, wall-time, custom
/// dimensions.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct UsageDimensions {
    /// Input tokens consumed (LLM-shaped usage). `0` if not applicable.
    pub input_tokens: u64,
    /// Output tokens produced (LLM-shaped usage).
    pub output_tokens: u64,
    /// Wall-clock duration, in milliseconds, attributable to this
    /// report. `None` for pure token-count reports.
    pub wall_ms: Option<u64>,
    /// Arbitrary caller-defined dimensions. Use `BTreeMap` for stable
    /// iteration order (important for dedup-key derivation on some
    /// budget schemes).
    pub custom: BTreeMap<String, u64>,
}

/// Admission outcome returned by `report_usage`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionDecision {
    /// Usage accepted; caller may continue.
    Admitted,
    /// Rate-limited or concurrency-capped; retry after the suggested
    /// interval.
    Throttled { retry_after_ms: u64 },
    /// Rejected outright — budget exhausted or policy violation.
    Rejected { reason: String },
}

// ── §3.3.0 Reclaim / lease types ────────────────────────────────────────

/// Opaque cookie returned by the reclaim scanner; consumed by
/// `claim_from_reclaim` to mint a resumed Handle.
///
/// Wraps [`ReclaimGrant`] today (the scanner's existing product).
/// Kept as a newtype so trait signatures name the reclaim-bound role
/// explicitly and so the wrapped shape can evolve without breaking the
/// trait.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReclaimToken {
    pub grant: ReclaimGrant,
}

impl ReclaimToken {
    pub fn new(grant: ReclaimGrant) -> Self {
        Self { grant }
    }
}

/// Result of a successful `renew` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LeaseRenewal {
    /// New lease expiry (monotonic ms).
    pub expires_at_ms: u64,
    /// Lease epoch after renewal. Monotonic non-decreasing.
    pub lease_epoch: u64,
}

impl LeaseRenewal {
    pub fn new(expires_at_ms: u64, lease_epoch: u64) -> Self {
        Self {
            expires_at_ms,
            lease_epoch,
        }
    }
}

// ── §3.3.0 Cancel-flow supporting types ─────────────────────────────────

/// Cancel-flow policy — what to do with the flow's members. Today
/// encoded as a `String` on `CancelFlowArgs`; Stage 0 extracts the
/// policy shape as a typed enum (RFC-012 §3.3.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelFlowPolicy {
    /// Cancel only the flow record; leave members alone.
    FlowOnly,
    /// Cancel the flow and every non-terminal member execution.
    CancelAll,
    /// Cancel the flow and every member currently in `Pending` /
    /// `Blocked` / `Eligible` — leave `Running` executions alone to
    /// drain.
    CancelPending,
}

/// Caller wait posture for `cancel_flow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelFlowWait {
    /// Return after the flow-state flip; member cancellations dispatch
    /// asynchronously.
    NoWait,
    /// Block until member cancellations complete, up to `timeout`.
    WaitTimeout(Duration),
    /// Block until member cancellations complete, no deadline.
    WaitIndefinite,
}

// ── §3.3.0 Completion stream payload ────────────────────────────────────

/// One completion event delivered through the `CompletionStream`
/// (RFC-012 §4.3). Also the payload type for issue #90's subscription
/// API. Stage 0 authorises the type; issue #90 fixes the wire shape.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompletionPayload {
    pub execution_id: crate::types::ExecutionId,
    pub outcome: String,
    pub payload_bytes: Option<Vec<u8>>,
    pub produced_at_ms: TimestampMs,
}

impl CompletionPayload {
    pub fn new(
        execution_id: crate::types::ExecutionId,
        outcome: impl Into<String>,
        payload_bytes: Option<Vec<u8>>,
        produced_at_ms: TimestampMs,
    ) -> Self {
        Self {
            execution_id,
            outcome: outcome.into(),
            payload_bytes,
            produced_at_ms,
        }
    }
}

// ── §3.3.0 ResumeSignal (crate move from ff-sdk::task) ──────────────────

/// Signal that satisfied a waitpoint matcher and is therefore part of
/// the reason an execution resumed. Returned by `observe_signals`
/// (RFC-012 §3.1.2) and by `ClaimedTask::resume_signals` in ff-sdk.
///
/// Moved in Stage 0 from `ff_sdk::task`; `ff_sdk::ResumeSignal` remains
/// re-exported through the 0.4.x window (removal scheduled for 0.5.0).
///
/// Returned only for signals whose matcher slot in the waitpoint's
/// resume condition is marked satisfied. Pre-buffered-but-unmatched
/// signals are not included.
///
/// Note: NOT `#[non_exhaustive]` to preserve struct-literal compatibility
/// with ff-sdk call sites that constructed `ResumeSignal { .. }` before
/// the Stage 0 crate move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeSignal {
    pub signal_id: crate::types::SignalId,
    pub signal_name: String,
    pub signal_category: String,
    pub source_type: String,
    pub source_identity: String,
    pub correlation_id: String,
    /// Valkey-server `now_ms` timestamp at which `ff_deliver_signal`
    /// accepted this signal. `0` if the stored `accepted_at` field is
    /// missing or non-numeric (a Lua-side defect — not expected at
    /// runtime).
    pub accepted_at: TimestampMs,
    /// Raw payload bytes, if the signal was delivered with one. `None`
    /// for signals delivered without a payload.
    pub payload: Option<Vec<u8>>,
}

// ── Stage 1a: FailOutcome move ──────────────────────────────────────────

/// Outcome of a `fail()` call.
///
/// **RFC-012 Stage 1a:** moved from `ff_sdk::task::FailOutcome` to
/// `ff_core::backend::FailOutcome` so it is nameable by the
/// `EngineBackend` trait signature. `ff_sdk::task` retains a
/// `pub use` shim preserving the `ff_sdk::FailOutcome` path.
///
/// Not `#[non_exhaustive]` because existing consumers (ff-test,
/// ff-readiness-tests) construct and match this enum exhaustively;
/// the shape has been stable since the `fail()` API landed and any
/// additive growth would be a follow-up RFC's deliberate break.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailOutcome {
    /// Retry was scheduled — execution is in delayed backoff.
    RetryScheduled {
        delay_until: crate::types::TimestampMs,
    },
    /// No retries left — execution is terminal failed.
    TerminalFailed,
}

// ── Stage 1a: BackendConfig + sub-types ─────────────────────────────────

/// Pool + keepalive timing shared across backend connections.
///
/// Fields are `#[non_exhaustive]` per project convention — connection
/// tunables grow as new backends land. Default is the Phase-1 Valkey
/// client's out-of-box shape (no explicit timeout, no explicit pool
/// cap; inherits ferriskey defaults).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendTimeouts {
    /// Per-request timeout. `None` ⇒ backend default.
    pub request: Option<Duration>,
    /// Idle-connection keepalive interval. `None` ⇒ backend default.
    pub keepalive: Option<Duration>,
}

/// Retry policy shared across backend connections. Additive; Stage 1a
/// ships the minimal shape so the trait signatures can reference it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendRetry {
    /// Max retry attempts on transient transport errors. `None` ⇒
    /// backend default.
    pub max_attempts: Option<u32>,
    /// Base backoff. `None` ⇒ backend default.
    pub base_backoff: Option<Duration>,
}

/// Valkey-specific connection parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ValkeyConnection {
    /// Valkey hostname.
    pub host: String,
    /// Valkey port.
    pub port: u16,
    /// Enable TLS for the Valkey connection.
    pub tls: bool,
    /// Enable Valkey cluster mode.
    pub cluster: bool,
}

impl ValkeyConnection {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: false,
            cluster: false,
        }
    }
}

/// Discriminated union over per-backend connection shapes. Stage 1a
/// ships the Valkey arm; future backends (Postgres) land additively
/// under the `#[non_exhaustive]` guard.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendConnection {
    Valkey(ValkeyConnection),
}

/// Configuration passed to `ValkeyBackend::connect` (and, later, to
/// other backend `connect` constructors). Carries the connection
/// details + shared timing/retry policy. Replaces the Valkey-specific
/// fields today on `WorkerConfig` (RFC-012 §5.1 migration plan).
///
/// `BackendConfig` is the replacement target for `WorkerConfig`'s
/// `host` / `port` / `tls` / `cluster` fields. The full migration
/// lands across Stage 1a (type introduction) and Stage 1c
/// (`WorkerConfig` forwarding); worker-policy fields (lease TTL,
/// claim poll interval, capability set) stay on `WorkerConfig`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendConfig {
    pub connection: BackendConnection,
    pub timeouts: BackendTimeouts,
    pub retry: BackendRetry,
}

impl BackendConfig {
    /// Build a Valkey BackendConfig from host+port. Other fields take
    /// backend defaults.
    pub fn valkey(host: impl Into<String>, port: u16) -> Self {
        Self {
            connection: BackendConnection::Valkey(ValkeyConnection::new(host, port)),
            timeouts: BackendTimeouts::default(),
            retry: BackendRetry::default(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::{Partition, PartitionFamily};
    use crate::types::{ExecutionId, LaneId, SignalId};

    #[test]
    fn backend_tag_derives() {
        let a = BackendTag::Valkey;
        let b = a;
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), "Valkey");
    }

    #[test]
    fn handle_kind_derives() {
        for k in [HandleKind::Fresh, HandleKind::Resumed, HandleKind::Suspended] {
            let c = k;
            assert_eq!(k, c);
            // Debug formatter reachable
            let _ = format!("{k:?}");
        }
    }

    #[test]
    fn handle_opaque_roundtrips() {
        let bytes: Box<[u8]> = Box::new([1u8, 2, 3, 4]);
        let o = HandleOpaque::new(bytes.clone());
        assert_eq!(o.as_bytes(), &[1u8, 2, 3, 4]);
        assert_eq!(o, o.clone());
        let _ = format!("{o:?}");
    }

    #[test]
    fn handle_composes() {
        let h = Handle::new(
            BackendTag::Valkey,
            HandleKind::Fresh,
            HandleOpaque::new(Box::new([0u8; 4])),
        );
        assert_eq!(h.backend, BackendTag::Valkey);
        assert_eq!(h.kind, HandleKind::Fresh);
        assert_eq!(h.clone(), h);
    }

    #[test]
    fn capability_set_derives() {
        let c1 = CapabilitySet::new(["gpu", "cuda"]);
        let c2 = CapabilitySet::new(["gpu", "cuda"]);
        assert_eq!(c1, c2);
        assert!(!c1.is_empty());
        assert!(CapabilitySet::default().is_empty());
        let _ = format!("{c1:?}");
    }

    #[test]
    fn claim_policy_derives() {
        let p = ClaimPolicy::with_max_wait(Duration::from_millis(500));
        assert_eq!(p.max_wait, Some(Duration::from_millis(500)));
        assert_eq!(p.clone(), p);
        assert_eq!(ClaimPolicy::immediate(), ClaimPolicy::default());
    }

    #[test]
    fn frame_and_kind_derive() {
        let f = Frame {
            bytes: b"hello".to_vec(),
            kind: FrameKind::Stdout,
            seq: Some(3),
        };
        assert_eq!(f.clone(), f);
        assert_eq!(f.kind, FrameKind::Stdout);
        assert_ne!(FrameKind::Stderr, FrameKind::Event);
        let _ = format!("{f:?}");
    }

    #[test]
    fn waitpoint_spec_derives() {
        let spec = WaitpointSpec {
            kind: WaitpointKind::SignalName,
            matcher: b"approved".to_vec(),
            hmac_token: WaitpointHmac::new("kid1:deadbeef"),
        };
        assert_eq!(spec.clone(), spec);
        assert_eq!(spec.hmac_token.as_str(), "kid1:deadbeef");
        assert_eq!(
            WaitpointHmac::new("a"),
            WaitpointHmac::new(String::from("a"))
        );
    }

    #[test]
    fn failure_reason_and_class() {
        let r1 = FailureReason::new("boom");
        let r2 = FailureReason::with_detail("boom", b"stack".to_vec());
        assert_eq!(r1.message, "boom");
        assert!(r1.detail.is_none());
        assert_eq!(r2.detail.as_deref(), Some(&b"stack"[..]));
        assert_eq!(r1.clone(), r1);
        assert_ne!(FailureClass::Transient, FailureClass::Permanent);
    }

    #[test]
    fn usage_dimensions_default_and_eq() {
        let u = UsageDimensions {
            input_tokens: 10,
            output_tokens: 20,
            wall_ms: Some(150),
            custom: BTreeMap::from([("net_bytes".to_string(), 42)]),
        };
        assert_eq!(u.clone(), u);
        assert_eq!(UsageDimensions::default().input_tokens, 0);
    }

    #[test]
    fn admission_decision_derives() {
        let a = AdmissionDecision::Admitted;
        let t = AdmissionDecision::Throttled { retry_after_ms: 25 };
        let r = AdmissionDecision::Rejected {
            reason: "quota".into(),
        };
        assert_eq!(a.clone(), a);
        assert_eq!(t.clone(), t);
        assert_ne!(a, r);
    }

    #[test]
    fn reclaim_token_wraps_grant() {
        let grant = ReclaimGrant {
            execution_id: ExecutionId::solo(&LaneId::new("default"), &Default::default()),
            partition: Partition {
                family: PartitionFamily::Flow,
                index: 0,
            },
            grant_key: "gkey".into(),
            expires_at_ms: 123,
            lane_id: LaneId::new("default"),
        };
        let t = ReclaimToken::new(grant.clone());
        assert_eq!(t.grant, grant);
        assert_eq!(t.clone(), t);
    }

    #[test]
    fn lease_renewal_is_copy() {
        let r = LeaseRenewal {
            expires_at_ms: 100,
            lease_epoch: 2,
        };
        let s = r; // Copy
        assert_eq!(r, s);
    }

    #[test]
    fn cancel_flow_policy_and_wait() {
        assert_ne!(CancelFlowPolicy::FlowOnly, CancelFlowPolicy::CancelAll);
        let w = CancelFlowWait::WaitTimeout(Duration::from_secs(1));
        assert_eq!(w, w);
        assert_ne!(CancelFlowWait::NoWait, CancelFlowWait::WaitIndefinite);
    }

    #[test]
    fn completion_payload_derives() {
        let c = CompletionPayload {
            execution_id: ExecutionId::solo(&LaneId::new("default"), &Default::default()),
            outcome: "success".into(),
            payload_bytes: Some(b"ok".to_vec()),
            produced_at_ms: TimestampMs::from_millis(1234),
        };
        assert_eq!(c.clone(), c);
        let _ = format!("{c:?}");
    }

    #[test]
    fn resume_signal_moved_and_derives() {
        let s = ResumeSignal {
            signal_id: SignalId::new(),
            signal_name: "approve".into(),
            signal_category: "decision".into(),
            source_type: "user".into(),
            source_identity: "u1".into(),
            correlation_id: "c1".into(),
            accepted_at: TimestampMs::from_millis(10),
            payload: None,
        };
        assert_eq!(s.clone(), s);
        let _ = format!("{s:?}");
    }

    #[test]
    fn fail_outcome_variants() {
        let retry = FailOutcome::RetryScheduled {
            delay_until: TimestampMs::from_millis(42),
        };
        let terminal = FailOutcome::TerminalFailed;
        assert_ne!(retry, terminal);
        assert_eq!(retry.clone(), retry);
    }

    #[test]
    fn backend_config_valkey_ctor() {
        let c = BackendConfig::valkey("host.local", 6379);
        // Same-crate match against an otherwise `#[non_exhaustive]`
        // enum is irrefutable — no wildcard needed and `let-else`
        // would trip `irrefutable_let_patterns`.
        let BackendConnection::Valkey(v) = &c.connection;
        assert_eq!(v.host, "host.local");
        assert_eq!(v.port, 6379);
        assert!(!v.tls);
        assert!(!v.cluster);
        assert_eq!(c.timeouts, BackendTimeouts::default());
        assert_eq!(c.retry, BackendRetry::default());
        assert_eq!(c.clone(), c);
    }
}
