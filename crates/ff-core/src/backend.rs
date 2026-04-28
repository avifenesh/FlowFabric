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

use crate::contracts::ResumeGrant;
use crate::types::{TimestampMs, WaitpointToken, WorkerId, WorkerInstanceId};
use serde::{Deserialize, Serialize};

// DX (HHH v0.3.4 re-smoke): `Namespace` lives in `ff_core::types` but
// is used on `BackendConfig` + `ScannerFilter` (both defined in this
// module). Re-export here so consumers already scoped to
// `ff_core::backend::*` can grab it without a second `use` line
// crossing into `ff_core::types`. Also brings `Namespace` into local
// scope for the definitions below.
pub use crate::types::Namespace;

// RFC-013 Stage 1d — re-export the typed suspend trait surface so
// external crates using `ff_core::backend::*` can reach the new types
// without a second `use ff_core::contracts` line. Keeps the trait
// naming surface coherent (`PendingWaitpoint`, `WaitpointSpec`, and
// the new `SuspendArgs` / `SuspendOutcome` all live at the same path).
pub use crate::contracts::{
    CompositeBody, IdempotencyKey, ResumeCondition, ResumePolicy, ResumeTarget, SignalMatcher,
    SuspendArgs, SuspendOutcome, SuspendOutcomeDetails, SuspensionReasonCode,
    SuspensionRequester, TimeoutBehavior, WaitpointBinding,
};
use std::collections::BTreeMap;
use std::time::Duration;

// ── §4.1 Handle trio ────────────────────────────────────────────────────

/// Backend-tag discriminator embedded in every [`Handle`] so ops can
/// dispatch to the correct backend implementation at runtime.
///
/// `#[non_exhaustive]`: new backend variants land additively as impls
/// come online (e.g. `Postgres` in Stage 2, hypothetical third backends
/// later).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BackendTag {
    /// The Valkey FCALL-backed implementation.
    Valkey,
    /// The Postgres-backed implementation (RFC-v0.7 Wave 1c).
    Postgres,
    /// The SQLite-backed implementation — dev-only (RFC-023).
    Sqlite,
}

impl BackendTag {
    /// Stable single-byte wire encoding for the tag. Embedded inside
    /// [`HandleOpaque`] so cross-backend migration tooling can detect
    /// which backend minted a given handle without parsing the rest of
    /// the payload (see [`crate::handle_codec`]).
    ///
    /// Wire byte allocations:
    /// * `0x01` — [`BackendTag::Valkey`]
    /// * `0x02` — [`BackendTag::Postgres`] (RFC-v0.7 Wave 1c)
    /// * `0x03` — [`BackendTag::Sqlite`] (RFC-023 Phase 2a.1.5)
    pub const fn wire_byte(self) -> u8 {
        match self {
            BackendTag::Valkey => 0x01,
            BackendTag::Postgres => 0x02,
            BackendTag::Sqlite => 0x03,
        }
    }

    /// Inverse of [`Self::wire_byte`].
    pub const fn from_wire_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(BackendTag::Valkey),
            0x02 => Some(BackendTag::Postgres),
            0x03 => Some(BackendTag::Sqlite),
            _ => None,
        }
    }
}

/// Lifecycle kind carried inside a [`Handle`]. Backends validate `kind`
/// on entry to each op and return `EngineError::State` on mismatch.
///
/// Replaces round-1's compile-time `Handle` / `ResumeHandle` /
/// `SuspendToken` type split (RFC-012 §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum HandleKind {
    /// Fresh claim — returned by `claim` / `claim_from_grant`.
    Fresh,
    /// Resumed from resume grant — returned by `claim_from_resume_grant`
    /// (renamed from `claim_from_reclaim` per RFC-024 PR-B+C).
    Resumed,
    /// Suspended — returned by `suspend`. Terminal for the lease;
    /// resumption mints a new Handle via `claim_from_resume_grant`.
    Suspended,
    /// Reclaimed from a lease-reclaim grant — returned by the new
    /// `reclaim_execution` trait method (RFC-024). Distinct from
    /// [`HandleKind::Resumed`] (suspend/resume path) and
    /// [`HandleKind::Fresh`] (first claim). The backing lifecycle
    /// creates a new attempt row and bumps the reclaim counter.
    Reclaimed,
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
/// attempt. Produced by `claim` / `claim_from_resume_grant` / `suspend`;
/// borrowed by every op (renew, progress, append_frame, complete, fail,
/// cancel, suspend, delay, wait_children, observe_signals, report_usage).
///
/// See RFC-012 §4.1 for the round-4 design — terminal ops borrow rather
/// than consume so callers can retry after a transport error.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Handle {
    pub backend: BackendTag,
    pub kind: HandleKind,
    pub opaque: HandleOpaque,
}

impl Handle {
    /// Construct a new Handle. Called by backend impls only; consumer
    /// code receives Handles from `claim` / `suspend` / `claim_from_resume_grant`.
    pub fn new(backend: BackendTag, kind: HandleKind, opaque: HandleOpaque) -> Self {
        Self {
            backend,
            kind,
            opaque,
        }
    }
}

/// Serde default for a `HandleKind::Fresh` stub handle on
/// [`crate::contracts::ClaimedExecution::handle`]. Used when
/// deserializing a legacy payload that predates the v0.12 PR-5.5 field
/// addition; live backends populate the field directly.
pub fn stub_handle_fresh() -> Handle {
    Handle::new(
        BackendTag::Valkey,
        HandleKind::Fresh,
        HandleOpaque::new(Box::new([])),
    )
}

/// Serde default for a `HandleKind::Resumed` stub handle on
/// [`crate::contracts::ClaimedResumedExecution::handle`].
pub fn stub_handle_resumed() -> Handle {
    Handle::new(
        BackendTag::Valkey,
        HandleKind::Resumed,
        HandleOpaque::new(Box::new([])),
    )
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

/// Policy hints for `claim`. Carries the worker identity the backend
/// needs to invoke `ff_claim_execution` (v0.7 Wave 2) plus the
/// blocking-wait bound.
///
/// **Wave 2 extension:** `worker_id` + `worker_instance_id` +
/// `lease_ttl_ms` are now required so the Valkey backend's `claim`
/// impl can issue the claim FCALL without a side-channel identity
/// lookup. The constructor change is a pre-1.0 breaking change,
/// tracked in the CHANGELOG.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClaimPolicy {
    /// Maximum blocking wait. `None` means backend-default (today:
    /// non-blocking / immediate return).
    pub max_wait: Option<Duration>,
    /// Worker identity (stable across process restarts).
    pub worker_id: WorkerId,
    /// Worker instance identity (unique per process).
    pub worker_instance_id: WorkerInstanceId,
    /// Lease TTL in milliseconds for the claim about to be minted.
    pub lease_ttl_ms: u32,
}

impl ClaimPolicy {
    /// Build a claim policy. `max_wait = None` means non-blocking /
    /// immediate return. `lease_ttl_ms` is the TTL the backend
    /// installs on the minted lease.
    pub fn new(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lease_ttl_ms: u32,
        max_wait: Option<Duration>,
    ) -> Self {
        Self {
            max_wait,
            worker_id,
            worker_instance_id,
            lease_ttl_ms,
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

// ── RFC-015 §1–§6: Stream-durability modes ──────────────────────────────

/// Patch format used by [`StreamMode::DurableSummary`] to apply each
/// frame's payload against the server-side rolling summary document.
///
/// v0.6 ships a single variant, `JsonMergePatch` (RFC 7396). The enum
/// is `#[non_exhaustive]` so the future `PatchKind::StringAppend`
/// variant flagged in RFC-015 §11 can land additively without a
/// breaking change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PatchKind {
    /// RFC 7396 JSON Merge Patch. Locked choice for v0.6 (RFC-015 §3.2).
    JsonMergePatch,
}

/// Per-call durability mode for [`EngineBackend::append_frame`].
///
/// RFC-015 §1. Mode is **per-call** — workers routinely mix frame types
/// (tokens + progress + final summary) in a single attempt and the right
/// durability differs per frame type. See RFC-015 §5 for the caveat on
/// mixed-mode streams.
///
/// `#[non_exhaustive]`: new modes land additively (the v0.6 PR deliberately
/// excludes `KeepAllDeltas`, dropped by owner adjudication; a future
/// mode for e.g. per-frame-replicated streams would land here).
///
/// ## Doc-comment contract on [`Self::Durable`]
///
/// If the same stream also receives [`Self::BestEffortLive`] frames,
/// `Durable` frames are subject to the best-effort MAXLEN trim (see
/// RFC-015 §5). Callers that need strict retention for `Durable` frames
/// alongside best-effort telemetry must not mix modes on one stream or
/// should place the durable frames on a sibling stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamMode {
    /// Current default — every frame XADDs as a durable entry.
    #[default]
    Durable,
    /// Server-side rolling-summary collapse. Each frame's payload is a
    /// delta/patch applied atomically to a summary Hash (per
    /// [`PatchKind`]); the delta is also XADDed to the stream with
    /// `mode=summary` fields so live tailers continue to observe change
    /// events. RFC-015 §3.
    DurableSummary { patch_kind: PatchKind },
    /// Short-lived frame. XADDed with `mode=best_effort`; per-stream
    /// MAXLEN rolls it off and the stream key gets a TTL refresh only
    /// when the stream has never held a durable frame (RFC-015 §4.1).
    ///
    /// The per-stream MAXLEN is computed dynamically in Lua from an
    /// EMA of observed append rate (RFC-015 §4.2). See
    /// [`BestEffortLiveConfig`] for the tunable knobs.
    BestEffortLive { config: BestEffortLiveConfig },
}

/// Configuration knobs for [`StreamMode::BestEffortLive`] — RFC-015
/// §4.2 dynamic MAXLEN sizing.
///
/// Defaults derived from the Phase 0 benchmark + RFC §4.1/§4.3 final
/// design:
///   - `ttl_ms` — caller-supplied visibility target.
///   - `maxlen_floor = 64` — RFC §4.1 round-2 default, used for
///     low-rate streams where the EMA formula would under-size.
///   - `maxlen_ceiling = 16_384` — cap on per-stream retained entries
///     (raised from the original §4.2 draft of 2048 after Phase 0
///     showed 200–4000 Hz LLM-token bursts saturate any lower clamp).
///   - `ema_alpha = 0.2` — RFC §4.3 gate value. Weights the latest
///     per-append instantaneous rate at 20 %, decays prior samples
///     at 80 % each append.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct BestEffortLiveConfig {
    pub ttl_ms: u32,
    pub maxlen_floor: u32,
    pub maxlen_ceiling: u32,
    pub ema_alpha: f64,
}

impl BestEffortLiveConfig {
    /// Construct with the given `ttl_ms` and defaults for the other
    /// knobs. Alias for [`Self::with_ttl`] — provided so external
    /// consumers have a conventional `new` constructor against this
    /// `#[non_exhaustive]` struct.
    pub fn new(ttl_ms: u32) -> Self {
        Self::with_ttl(ttl_ms)
    }

    /// Construct with the given `ttl_ms` and defaults for the other
    /// knobs. Matches the shorthand used by
    /// [`StreamMode::best_effort_live`].
    pub fn with_ttl(ttl_ms: u32) -> Self {
        Self {
            ttl_ms,
            maxlen_floor: 64,
            maxlen_ceiling: 16_384,
            ema_alpha: 0.2,
        }
    }

    /// Builder: override [`Self::maxlen_floor`]. Chainable.
    pub fn with_maxlen_floor(mut self, floor: u32) -> Self {
        self.maxlen_floor = floor;
        self
    }

    /// Builder: override [`Self::maxlen_ceiling`]. Chainable.
    pub fn with_maxlen_ceiling(mut self, ceiling: u32) -> Self {
        self.maxlen_ceiling = ceiling;
        self
    }

    /// Builder: override [`Self::ema_alpha`]. Chainable. Callers are
    /// expected to keep α in `(0.0, 1.0]`; the Lua side clamps to that
    /// range defensively.
    pub fn with_ema_alpha(mut self, alpha: f64) -> Self {
        self.ema_alpha = alpha;
        self
    }
}

// Eq/Hash on an f64 field is unsafe (NaN); derive only what's sound.
impl Eq for BestEffortLiveConfig {}
impl std::hash::Hash for BestEffortLiveConfig {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ttl_ms.hash(state);
        self.maxlen_floor.hash(state);
        self.maxlen_ceiling.hash(state);
        self.ema_alpha.to_bits().hash(state);
    }
}

impl StreamMode {
    /// The v0.6 default — [`Self::Durable`]. Provided for symmetry with
    /// the other constructors (the enum is `#[non_exhaustive]` so
    /// cross-crate consumers cannot construct variants by name).
    pub fn durable() -> Self {
        StreamMode::Durable
    }

    /// [`Self::DurableSummary`] with [`PatchKind::JsonMergePatch`] —
    /// the only supported `PatchKind` in v0.6.
    pub fn durable_summary() -> Self {
        StreamMode::DurableSummary {
            patch_kind: PatchKind::JsonMergePatch,
        }
    }

    /// [`Self::BestEffortLive`] with the caller-supplied `ttl_ms` and
    /// default [`BestEffortLiveConfig`] knobs.
    /// RFC-015 §4 guidance: `5_000..=30_000` ms. Below ~1000 ms a live
    /// tailer may not connect in time; above ~60_000 ms the memory
    /// argument against plain [`Self::Durable`] weakens.
    pub fn best_effort_live(ttl_ms: u32) -> Self {
        StreamMode::BestEffortLive {
            config: BestEffortLiveConfig::with_ttl(ttl_ms),
        }
    }

    /// [`Self::BestEffortLive`] with a fully-specified
    /// [`BestEffortLiveConfig`]. Use for per-workload tuning of α, the
    /// MAXLEN clamp, or the TTL — defaults are wired from
    /// [`BestEffortLiveConfig::with_ttl`].
    pub fn best_effort_live_with_config(config: BestEffortLiveConfig) -> Self {
        StreamMode::BestEffortLive { config }
    }

    /// Stable wire-level token for this mode, written to the XADD entry
    /// `mode` field (RFC-015 §6.1). Tail filters compare against these
    /// string values server-side.
    pub fn wire_str(&self) -> &'static str {
        match self {
            StreamMode::Durable => "durable",
            StreamMode::DurableSummary { .. } => "summary",
            StreamMode::BestEffortLive { .. } => "best_effort",
        }
    }
}

/// Tail-stream visibility filter (RFC-015 §6).
///
/// Default [`Self::All`] preserves v1 behaviour; opt-in
/// [`Self::ExcludeBestEffort`] filters out `BestEffortLive` frames on
/// the server side (the XADD `mode` field is a cheap field check in
/// `ff_read_attempt_stream` / `xread_block`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TailVisibility {
    /// Default. Returns every XADD entry in the stream regardless of
    /// mode.
    #[default]
    All,
    /// Returns only frames appended under [`StreamMode::Durable`] or
    /// [`StreamMode::DurableSummary`] (i.e. filters out
    /// [`StreamMode::BestEffortLive`]). Named to be self-describing:
    /// the filter excludes best-effort frames, not "only Durable" —
    /// `DurableSummary` deltas are included because they have a
    /// durable backing (the summary Hash).
    ExcludeBestEffort,
}

impl TailVisibility {
    /// Stable wire-level token pushed as an ARGV to the Lua tail/read
    /// implementations. `""` (empty) means default = `All` (no filter).
    pub fn wire_str(&self) -> &'static str {
        match self {
            TailVisibility::All => "",
            TailVisibility::ExcludeBestEffort => "exclude_best_effort",
        }
    }
}

/// Byte-exact null-sentinel used by [`StreamMode::DurableSummary`] +
/// [`PatchKind::JsonMergePatch`] to set a field to JSON `null` (RFC-015
/// §3.2).
///
/// RFC 7396 treats `null` as "delete the key", so callers that actually
/// want a literal JSON `null` leaf send this sentinel string as the
/// scalar leaf value; the Lua apply-path rewrites the sentinel to JSON
/// `null` after the merge.
///
/// # Round-trip invariant
///
/// The summary document — as returned by a `read_summary` call — NEVER
/// contains the sentinel string. Any `null` observed always means "this
/// field is explicitly null." See [`SummaryDocument`].
pub const SUMMARY_NULL_SENTINEL: &str = "__ff_null__";

/// Materialised rolling summary document returned by a summary-read
/// call (RFC-015 §6.3).
///
/// `document` is the caller-owned JSON value; scalar leaves that the
/// caller wrote through the [`SUMMARY_NULL_SENTINEL`] contract appear
/// here as JSON `null` (not as the sentinel string — the round-trip
/// invariant erases the sentinel on read).
///
/// `#[non_exhaustive]` keeps future additive fields (e.g. a compacted
/// delta cursor, a schema-version tag) additive. Construct via
/// [`Self::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SummaryDocument {
    /// The rolling summary as JSON bytes (UTF-8, encoded by the
    /// server-side applier after merge). Stored as `Vec<u8>` instead
    /// of `serde_json::Value` to keep `ff_core::backend` free of a
    /// public `serde_json` type dependency.
    pub document_json: Vec<u8>,
    /// Monotonic version bumped on every delta applied. `0` is never
    /// observed — the first applied delta returns `1`.
    pub version: u64,
    /// Which [`PatchKind`] was used to build the document.
    pub patch_kind: PatchKind,
    /// Unix millis of the most recent delta application.
    pub last_updated_ms: u64,
    /// Unix millis of the first delta applied to this attempt.
    pub first_applied_ms: u64,
}

impl SummaryDocument {
    /// Build a summary snapshot. Used by backends parsing the Hash
    /// fields; external consumers receive these from `read_summary`.
    pub fn new(
        document_json: Vec<u8>,
        version: u64,
        patch_kind: PatchKind,
        last_updated_ms: u64,
        first_applied_ms: u64,
    ) -> Self {
        Self {
            document_json,
            version,
            patch_kind,
            last_updated_ms,
            first_applied_ms,
        }
    }
}

/// Single stream frame appended via `append_frame` (RFC-012 §3.3.0).
/// Today's FCALL takes the byte payload + frame_type + optional seq as
/// discrete ARGV; Stage 0 collects them into a named type for trait
/// signatures.
///
/// **Round-7 follow-up (PR #145 → #146):** extended with
/// `frame_type: String` (the SDK-public free-form classifier — values
/// like `"delta"`, `"log"`, `"agent_step"`, `"summary_token"`,
/// `"transcribe_line"`, `"progress"` — distinct from the coarse
/// [`FrameKind`] enum) and `correlation_id: Option<String>` (the
/// wire-level `correlation_id` ARGV, surfaced at the SDK as
/// `metadata: Option<&str>`). Adding these lets
/// `ClaimedTask::append_frame` forward through the trait without
/// wire-parity regression.
///
/// `frame_type` is free-form and is what the backend writes into the
/// Lua-side `frame_type` ARGV. [`FrameKind`] remains for typed
/// classification at the trait surface; when callers populate only
/// `kind`, the backend falls back to a stable encoding of the enum
/// variant (see `frame_kind_to_str` in `ff-backend-valkey`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub kind: FrameKind,
    /// Optional monotonic sequence. Set by the caller when the stream
    /// protocol is sequence-bound; `None` lets the backend assign.
    pub seq: Option<u64>,
    /// Free-form classifier written to the Lua-side `frame_type` ARGV.
    /// Empty string means "defer to [`FrameKind`]" — the backend
    /// substitutes the enum-variant encoding.
    pub frame_type: String,
    /// Optional correlation id (wire `correlation_id` ARGV). `None`
    /// encodes as the empty string on the wire.
    pub correlation_id: Option<String>,
    /// Durability mode for this frame (RFC-015 §1). Defaults to
    /// [`StreamMode::Durable`] for v1-caller parity — the field was
    /// added additively so pre-RFC-015 construction via
    /// [`Self::new`] / [`Self::with_seq`] sees `Durable` without code
    /// change.
    pub mode: StreamMode,
}

impl Frame {
    /// Construct a frame. `seq` defaults to `None` (backend-assigned);
    /// `frame_type` defaults to empty (backend falls back to
    /// `FrameKind` encoding); `correlation_id` defaults to `None`.
    /// Callers that need an explicit sequence use [`Frame::with_seq`];
    /// callers on the SDK forwarder path populate `frame_type` +
    /// `correlation_id` via [`Frame::with_frame_type`] /
    /// [`Frame::with_correlation_id`].
    pub fn new(bytes: Vec<u8>, kind: FrameKind) -> Self {
        Self {
            bytes,
            kind,
            seq: None,
            frame_type: String::new(),
            correlation_id: None,
            mode: StreamMode::Durable,
        }
    }

    /// Construct a frame with an explicit monotonic sequence.
    pub fn with_seq(bytes: Vec<u8>, kind: FrameKind, seq: u64) -> Self {
        Self {
            bytes,
            kind,
            seq: Some(seq),
            frame_type: String::new(),
            correlation_id: None,
            mode: StreamMode::Durable,
        }
    }

    /// Builder-style setter for the RFC-015 durability [`StreamMode`].
    /// Defaults to [`StreamMode::Durable`] (v1 parity) when unset.
    pub fn with_mode(mut self, mode: StreamMode) -> Self {
        self.mode = mode;
        self
    }

    /// Builder-style setter for the free-form `frame_type` classifier.
    pub fn with_frame_type(mut self, frame_type: impl Into<String>) -> Self {
        self.frame_type = frame_type.into();
        self
    }

    /// Builder-style setter for the optional `correlation_id`.
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
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

/// Handle returned by `create_waitpoint` — the id of the newly-minted
/// pending waitpoint plus its HMAC token. Signals targeted at the
/// waitpoint must present the token; a later `suspend` call transitions
/// the waitpoint from `pending` to `active` (RFC-012 §R7.2.2).
///
/// `WaitpointHmac` redacts on `Debug`/`Display`, so deriving `Debug`
/// here cannot leak the raw digest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingWaitpoint {
    pub waitpoint_id: crate::types::WaitpointId,
    pub hmac_token: WaitpointHmac,
}

impl PendingWaitpoint {
    pub fn new(waitpoint_id: crate::types::WaitpointId, hmac_token: WaitpointHmac) -> Self {
        Self {
            waitpoint_id,
            hmac_token,
        }
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
    /// Optional caller-supplied idempotency key. When set, the backend
    /// rejects a repeat application of the same key with
    /// `ReportUsageResult::AlreadyApplied` rather than double-counting
    /// (RFC-012 §R7.4; Lua `ff_report_usage_and_check` threads this as
    /// the trailing ARGV). `None` / empty string disables dedup.
    pub dedup_key: Option<String>,
}

impl UsageDimensions {
    /// Create an empty usage report (all dimensions zero / `None`).
    ///
    /// Provided for external-crate consumers: `UsageDimensions` is
    /// `#[non_exhaustive]`, so struct-literal and functional-update
    /// construction are unavailable across crate boundaries. Start
    /// from `new()` and chain `with_*` setters to build a report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the input-token count dimension. Consumes and returns
    /// `self` for chaining.
    pub fn with_input_tokens(mut self, tokens: u64) -> Self {
        self.input_tokens = tokens;
        self
    }

    /// Set the output-token count dimension. Consumes and returns
    /// `self` for chaining.
    pub fn with_output_tokens(mut self, tokens: u64) -> Self {
        self.output_tokens = tokens;
        self
    }

    /// Set the wall-clock duration dimension, in milliseconds.
    /// Consumes and returns `self` for chaining.
    pub fn with_wall_ms(mut self, ms: u64) -> Self {
        self.wall_ms = Some(ms);
        self
    }

    /// Set the optional caller-supplied idempotency key. When set,
    /// the backend rejects a repeat application of the same key with
    /// `ReportUsageResult::AlreadyApplied` rather than double-counting
    /// (RFC-012 §R7.4). Consumes and returns `self` for chaining.
    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }
}

// ── §3.3.0 Resume / lease types ────────────────────────────────────────

/// Opaque cookie returned by the reclaim scanner; consumed by
/// [`crate::engine_backend::EngineBackend::claim_from_resume_grant`]
/// to mint a resumed Handle.
///
/// Wraps [`ResumeGrant`] today (the scanner's existing product).
/// Kept as a newtype so trait signatures name the resume-bound role
/// explicitly and so the wrapped shape can evolve without breaking the
/// trait.
///
/// **Naming history (RFC-024).** This type was historically called
/// `ReclaimToken`. Its semantic is resume-after-suspend (it wraps a
/// `ResumeGrant` and feeds `ff_claim_resumed_execution`), so
/// RFC-024 Rev 2 renamed it to `ResumeToken`. The transitional
/// `ReclaimToken` alias lived for one PR and was dropped in PR-B+C;
/// downstream call sites migrate to `ResumeToken`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResumeToken {
    pub grant: ResumeGrant,
    /// Worker identity that will claim the resumed execution.
    /// Wave 2 additive extension — mirrors the `ClaimPolicy`
    /// shape since `claim_from_resume_grant` does not take a
    /// `ClaimPolicy`.
    pub worker_id: WorkerId,
    /// Worker instance identity.
    pub worker_instance_id: WorkerInstanceId,
    /// Lease TTL (ms) for the resumed attempt's fresh lease.
    pub lease_ttl_ms: u32,
}

impl ResumeToken {
    pub fn new(
        grant: ResumeGrant,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lease_ttl_ms: u32,
    ) -> Self {
        Self {
            grant,
            worker_id,
            worker_instance_id,
            lease_ttl_ms,
        }
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
///
/// `flow_id` was added in issue #90 so DAG-dependency routing
/// (`dispatch_dependency_resolution`) has the partition-routable flow
/// handle without reparsing the Lua-emitted JSON downstream.
/// `#[non_exhaustive]` keeps future field additions additive; use
/// [`CompletionPayload::new`] and [`CompletionPayload::with_flow_id`]
/// for construction.
///
/// `payload_bytes` / `produced_at_ms` are authorised but not yet
/// populated by the Valkey Lua emitters — consumers on the
/// `CompletionStream` read `execution_id` + `flow_id` today and must
/// tolerate `payload_bytes = None` / `produced_at_ms = 0`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompletionPayload {
    pub execution_id: crate::types::ExecutionId,
    pub outcome: String,
    pub payload_bytes: Option<Vec<u8>>,
    pub produced_at_ms: TimestampMs,
    /// Flow handle for partition routing. Added in issue #90 (#90);
    /// `None` for emitters that don't yet surface it.
    pub flow_id: Option<crate::types::FlowId>,
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
            flow_id: None,
        }
    }

    /// Attach a flow handle to the payload. Additive builder so adding
    /// `flow_id` didn't require a breaking change to [`Self::new`].
    #[must_use]
    pub fn with_flow_id(mut self, flow_id: crate::types::FlowId) -> Self {
        self.flow_id = Some(flow_id);
        self
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

// ── Issue #281: PrepareOutcome (boot-prep trait-method return) ────────

/// Outcome of an [`EngineBackend::prepare`](crate::engine_backend::EngineBackend::prepare)
/// call — one-time backend-specific boot preparation.
///
/// Issue #281: moves cairn's `ensure_library` retry loop upstream so
/// consumers can boot any backend uniformly via
/// `backend.prepare().await?` without knowing whether it is Valkey
/// (FUNCTION LOAD) or Postgres (migrations are out-of-band per
/// RFC-v0.7 Wave 0 Q12, so `NoOp`). `#[non_exhaustive]` so future
/// backends can add variants (e.g. `Skipped { reason }`) additively.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrepareOutcome {
    /// Backend had nothing to do — either genuinely no-op (Postgres:
    /// migrations applied out-of-band) or the requested prep work was
    /// already in place and idempotent (Valkey: library already at
    /// the expected version; the Valkey impl collapses the
    /// "already-present" case into `Applied` to keep one success
    /// variant — consumers that want to distinguish can parse
    /// `description`).
    NoOp,
    /// Backend ran preparation work (e.g. Valkey FUNCTION LOAD
    /// REPLACE). `description` is a human-readable summary suitable
    /// for `info!`-level log lines like
    /// `"FUNCTION LOAD (flowfabric lib v<N>)"`; shape is not
    /// machine-parseable and MAY change across versions.
    Applied {
        /// Human-readable summary of what was prepared.
        description: String,
    },
}

impl PrepareOutcome {
    /// Shorthand constructor for the `Applied` variant.
    pub fn applied(description: impl Into<String>) -> Self {
        Self::Applied {
            description: description.into(),
        }
    }
}

// ── RFC-012 §R7: AppendFrameOutcome move ────────────────────────────────

/// Outcome of an `append_frame()` call.
///
/// **RFC-012 §R7.2.1:** moved from `ff_sdk::task::AppendFrameOutcome`
/// to `ff_core::backend::AppendFrameOutcome` so it is nameable by the
/// `EngineBackend::append_frame` trait return. `ff_sdk::task` retains
/// a `pub use` shim preserving the `ff_sdk::task::AppendFrameOutcome`
/// path through 0.4.x.
///
/// Derive set matches the `FailOutcome` precedent
/// (`Clone, Debug, PartialEq, Eq`). Not `#[non_exhaustive]`:
/// construction is internal to the backend today (parser in
/// `ff-backend-valkey`), and no external constructors are anticipated
/// (consumer-shape evidence per §R7.2.1 / MN3).
///
/// `stream_id: String` is a stable shape commitment — a future typed
/// `StreamId` newtype would be its own breaking change (§R7.5.6 / MD2).
///
/// RFC-015 §9 made this type `#[non_exhaustive]` so the new
/// `summary_version: Option<u64>` field (populated only for
/// [`StreamMode::DurableSummary`] appends) can land additively and
/// future per-mode outcome fields can follow. Construct via
/// [`Self::new`] + the chainable setters; cross-crate consumers cannot
/// use struct-literal construction.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AppendFrameOutcome {
    /// Valkey Stream entry ID assigned to this frame (e.g. `1234567890-0`).
    pub stream_id: String,
    /// Total frame count in the stream after this append.
    pub frame_count: u64,
    /// Rolling summary `version` after the delta applied, populated
    /// only for [`StreamMode::DurableSummary`] appends (RFC-015 §3.3
    /// step 6). `None` for [`StreamMode::Durable`] /
    /// [`StreamMode::BestEffortLive`] appends — callers that need
    /// "total deltas applied" read this field.
    pub summary_version: Option<u64>,
}

impl AppendFrameOutcome {
    /// Build an outcome with the mandatory `stream_id` / `frame_count`
    /// fields. `summary_version` defaults to `None` — call
    /// [`Self::with_summary_version`] for [`StreamMode::DurableSummary`]
    /// appends.
    pub fn new(stream_id: impl Into<String>, frame_count: u64) -> Self {
        Self {
            stream_id: stream_id.into(),
            frame_count,
            summary_version: None,
        }
    }

    /// Attach a rolling summary version (RFC-015 §9).
    #[must_use]
    pub fn with_summary_version(mut self, version: u64) -> Self {
        self.summary_version = Some(version);
        self
    }
}

// ── Stage 1a: BackendConfig + sub-types ─────────────────────────────────

/// Pool timing shared across backend connections.
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
}

impl BackendTimeouts {
    /// Construct a [`BackendTimeouts`] with all fields set to their
    /// backend-default sentinel (`None`). Equivalent to
    /// [`Self::default`] — provided so external consumers have an
    /// explicit constructor against this `#[non_exhaustive]` struct.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Retry policy shared across backend connections.
///
/// Matches ferriskey's `ConnectionRetryStrategy` shape (see
/// `ferriskey/src/client/types.rs:151`) so Stage 1c's Valkey wiring is a
/// direct pass-through — we don't reimplement what ferriskey already
/// provides. The Postgres backend (future) interprets the same fields
/// under its own retry semantics, or maps `None` to its own defaults.
///
/// Each field is `Option<u32>`: `None` ⇒ backend default (for Valkey,
/// this means `ConnectionRetryStrategy::default()`); `Some(v)` ⇒ pass
/// `v` straight through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendRetry {
    /// Exponent base for the backoff curve. `None` ⇒ backend default.
    pub exponent_base: Option<u32>,
    /// Multiplicative factor applied to each backoff step. `None` ⇒
    /// backend default.
    pub factor: Option<u32>,
    /// Maximum number of retry attempts on transient transport errors.
    /// `None` ⇒ backend default.
    pub number_of_retries: Option<u32>,
    /// Jitter as a percentage of the computed backoff. `None` ⇒ backend
    /// default.
    pub jitter_percent: Option<u32>,
}

impl BackendRetry {
    /// Construct a [`BackendRetry`] with all fields set to their
    /// backend-default sentinel (`None`). Equivalent to
    /// [`Self::default`] — provided so external consumers have an
    /// explicit constructor against this `#[non_exhaustive]` struct.
    pub fn new() -> Self {
        Self::default()
    }
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

/// Postgres-specific connection parameters (RFC-v0.7 Wave 0).
///
/// `url` is a standard libpq/sqlx connection string
/// (`postgres://user:pass@host:port/db`). Pool sizing + acquire
/// timeout are carried on this struct rather than
/// [`BackendTimeouts`] / [`BackendRetry`] because they map directly
/// to `sqlx::postgres::PgPoolOptions` and have no Valkey equivalent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostgresConnection {
    /// Postgres connection URL.
    pub url: String,
    /// Maximum number of pool connections.
    pub max_connections: u32,
    /// Minimum number of idle pool connections.
    pub min_connections: u32,
    /// Per-acquire pool timeout.
    pub acquire_timeout: Duration,
}

impl PostgresConnection {
    /// Build a Postgres connection from a URL with defaults for pool
    /// sizing (matches sqlx's out-of-box defaults: max=10, min=0,
    /// acquire=30s).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
        }
    }
}

/// Discriminated union over per-backend connection shapes. Stage 1a
/// shipped the Valkey arm; RFC-v0.7 Wave 0 adds the Postgres arm
/// additively under the `#[non_exhaustive]` guard.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendConnection {
    Valkey(ValkeyConnection),
    Postgres(PostgresConnection),
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
    /// Construct a [`BackendConfig`] directly from a
    /// [`BackendConnection`] variant, with default timeouts + retry.
    /// Provided so external consumers have an explicit constructor
    /// against this `#[non_exhaustive]` struct; for the common paths
    /// prefer [`Self::valkey`] / [`Self::postgres`].
    pub fn new(connection: BackendConnection) -> Self {
        Self {
            connection,
            timeouts: BackendTimeouts::default(),
            retry: BackendRetry::default(),
        }
    }

    /// Build a Valkey BackendConfig from host+port. Other fields take
    /// backend defaults.
    pub fn valkey(host: impl Into<String>, port: u16) -> Self {
        Self {
            connection: BackendConnection::Valkey(ValkeyConnection::new(host, port)),
            timeouts: BackendTimeouts::default(),
            retry: BackendRetry::default(),
        }
    }

    /// Build a Postgres BackendConfig from a URL. Other fields take
    /// backend defaults. RFC-v0.7 Wave 0.
    pub fn postgres(url: impl Into<String>) -> Self {
        Self {
            connection: BackendConnection::Postgres(PostgresConnection::new(url)),
            timeouts: BackendTimeouts::default(),
            retry: BackendRetry::default(),
        }
    }
}

// ── Issue #122: ScannerFilter ───────────────────────────────────────────

/// Per-consumer filter applied by FlowFabric's background scanners and
/// completion subscribers so multiple FlowFabric instances sharing a
/// single Valkey keyspace can operate on disjoint subsets of
/// executions without mutual interference (issue #122).
///
/// Sibling of [`CompletionPayload`]: the former is a scan *output*
/// shape, this is the *input* predicate scanners and completion
/// subscribers consult per candidate.
///
/// # Fields & backing storage
///
/// * `namespace` — matches against the `namespace` field on
///   `exec_core` (Valkey hash `ff:exec:{fp:N}:<eid>:core`). Cost:
///   one HGET per candidate when set.
/// * `instance_tag` — matches against an entry in the execution's
///   user-supplied tags hash at the canonical key
///   **`ff:exec:{p}:<eid>:tags`** (where `{p}` is the partition
///   hash-tag, e.g. `{fp:42}`). Written by the Lua function
///   `ff_create_execution` (see `lua/execution.lua`) and
///   `ff_set_execution_tags`. The tuple is `(tag_key, tag_value)`:
///   the HGET targets `tag_key` on the tags hash and compares the
///   returned string (if any) byte-for-byte against `tag_value`.
///   Cost: one HGET per candidate when set.
///
/// When both are set, scanners check `namespace` first (short-circuit
/// on mismatch) then `instance_tag`, for a maximum of 2 HGETs per
/// candidate.
///
/// # Semantics
///
/// * [`Self::is_noop`] returns true when both fields are `None` —
///   the filter accepts every candidate. Used by the
///   `subscribe_completions_filtered` default body to fall back to
///   the unfiltered subscription.
/// * [`Self::matches`] is the tight in-memory predicate once the
///   HGET values have been fetched. Scanners fetch the fields
///   lazily (namespace first) and pass the results in; the helper
///   returns false as soon as one component mismatches.
///
/// # Scope
///
/// Today the filter is consulted by execution-shaped scanners
/// (lease_expiry, attempt_timeout, execution_deadline,
/// suspension_timeout, pending_wp_expiry, delayed_promoter,
/// dependency_reconciler, cancel_reconciler, unblock,
/// index_reconciler, retention_trimmer) and by completion
/// subscribers. Non-execution scanners (budget_reconciler,
/// budget_reset, quota_reconciler, flow_projector) accept a filter
/// for API uniformity but do not apply it — their iteration domains
/// (budgets, quotas, flows) are not keyed by the
/// per-execution namespace / instance_tag shape.
///
/// `#[non_exhaustive]` so future dimensions (e.g. `lane_id`,
/// `worker_instance`) can land additively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScannerFilter {
    /// Tenant / workspace scope. Matches against the `namespace`
    /// field on `exec_core`.
    pub namespace: Option<Namespace>,
    /// Instance-scoped tag predicate `(tag_key, tag_value)`. Matches
    /// against an entry in `ff:exec:{p}:<eid>:tags` (the tags hash
    /// written by `ff_create_execution`).
    pub instance_tag: Option<(String, String)>,
}

impl ScannerFilter {
    /// Shared no-op filter — useful as the default for the
    /// `Scanner::filter()` trait method so implementors that don't
    /// override can hand back a `&'static` reference without
    /// allocating per call.
    pub const NOOP: Self = Self {
        namespace: None,
        instance_tag: None,
    };

    /// Create an empty filter (equivalent to [`Self::NOOP`]).
    ///
    /// Provided for external-crate consumers: `ScannerFilter` is
    /// `#[non_exhaustive]`, so struct-literal and functional-update
    /// construction are unavailable across crate boundaries. Start
    /// from `new()` and chain `with_*` setters to build a filter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tenant-scope namespace filter dimension. Consumes
    /// and returns `self` for chaining.
    ///
    /// Accepts anything that converts into [`Namespace`] so callers
    /// can pass `&str` / `String` directly without the
    /// `Namespace::new(...)` ceremony (the conversion is infallible;
    /// [`Namespace`] has `From<&str>` and `From<String>` via the
    /// crate's `string_id!` macro).
    pub fn with_namespace(mut self, ns: impl Into<Namespace>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Set the exact-match exec-tag filter dimension. Consumes and
    /// returns `self` for chaining. At filter time, scanners read
    /// the `ff:exec:{p:N}:<eid>:tags` hash and compare the value at
    /// `key` byte-for-byte against `value`.
    pub fn with_instance_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.instance_tag = Some((key.into(), value.into()));
        self
    }

    /// True iff the filter has no dimensions set — every candidate
    /// passes. Callers use this to short-circuit filtered subscribe
    /// paths back to the unfiltered ones.
    pub fn is_noop(&self) -> bool {
        self.namespace.is_none() && self.instance_tag.is_none()
    }

    /// Post-HGET in-memory match check.
    ///
    /// `core_namespace` should be the `namespace` field read from
    /// `exec_core` (None if the HGET returned nil / was skipped).
    /// `tag_value` should be the HGET result for the configured
    /// `instance_tag.0` key on the execution's tags hash (None if
    /// the HGET returned nil / was skipped).
    ///
    /// When a filter dimension is `None` the corresponding argument
    /// is ignored — callers may pass `None` to skip the HGET and
    /// save a round-trip.
    pub fn matches(
        &self,
        core_namespace: Option<&Namespace>,
        tag_value: Option<&str>,
    ) -> bool {
        if let Some(ref want) = self.namespace {
            match core_namespace {
                Some(have) if have == want => {}
                _ => return false,
            }
        }
        if let Some((_, ref want_value)) = self.instance_tag {
            match tag_value {
                Some(have) if have == want_value.as_str() => {}
                _ => return false,
            }
        }
        true
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
        let p = ClaimPolicy::new(
            WorkerId::new("w"),
            WorkerInstanceId::new("w-1"),
            30_000,
            Some(Duration::from_millis(500)),
        );
        assert_eq!(p.max_wait, Some(Duration::from_millis(500)));
        assert_eq!(p.lease_ttl_ms, 30_000);
        assert_eq!(p.worker_id.as_str(), "w");
        assert_eq!(p.worker_instance_id.as_str(), "w-1");
        assert_eq!(p.clone(), p);
        let immediate = ClaimPolicy::new(
            WorkerId::new("w"),
            WorkerInstanceId::new("w-1"),
            30_000,
            None,
        );
        assert_eq!(immediate.max_wait, None);
    }

    #[test]
    fn frame_and_kind_derive() {
        let f = Frame {
            bytes: b"hello".to_vec(),
            kind: FrameKind::Stdout,
            seq: Some(3),
            frame_type: "delta".to_owned(),
            correlation_id: Some("req-42".to_owned()),
            mode: StreamMode::Durable,
        };
        assert_eq!(f.clone(), f);
        assert_eq!(f.kind, FrameKind::Stdout);
        assert_eq!(f.frame_type, "delta");
        assert_eq!(f.correlation_id.as_deref(), Some("req-42"));
        assert_ne!(FrameKind::Stderr, FrameKind::Event);
        let _ = format!("{f:?}");
    }

    #[test]
    fn frame_builders_populate_extended_fields() {
        let f = Frame::new(b"payload".to_vec(), FrameKind::Event)
            .with_frame_type("agent_step")
            .with_correlation_id("corr-1");
        assert_eq!(f.frame_type, "agent_step");
        assert_eq!(f.correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(f.seq, None);

        let bare = Frame::new(b"p".to_vec(), FrameKind::Event);
        assert_eq!(bare.frame_type, "");
        assert_eq!(bare.correlation_id, None);
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
            dedup_key: Some("k1".into()),
        };
        assert_eq!(u.clone(), u);
        assert_eq!(UsageDimensions::default().input_tokens, 0);
        assert_eq!(UsageDimensions::default().dedup_key, None);
    }

    #[test]
    fn usage_dimensions_builder_chain() {
        let u = UsageDimensions::new()
            .with_input_tokens(10)
            .with_output_tokens(20)
            .with_wall_ms(150)
            .with_dedup_key("k1");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.wall_ms, Some(150));
        assert_eq!(u.dedup_key.as_deref(), Some("k1"));
        assert!(u.custom.is_empty());
        // `new()` is equivalent to `default()`.
        assert_eq!(UsageDimensions::new(), UsageDimensions::default());
    }

    #[test]
    fn reclaim_token_wraps_grant() {
        let grant = ResumeGrant {
            execution_id: ExecutionId::solo(&LaneId::new("default"), &Default::default()),
            partition_key: crate::partition::PartitionKey::from(&Partition {
                family: PartitionFamily::Flow,
                index: 0,
            }),
            grant_key: "gkey".into(),
            expires_at_ms: 123,
            lane_id: LaneId::new("default"),
        };
        let t = ResumeToken::new(
            grant.clone(),
            WorkerId::new("w"),
            WorkerInstanceId::new("w-1"),
            30_000,
        );
        assert_eq!(t.grant, grant);
        assert_eq!(t.worker_id.as_str(), "w");
        assert_eq!(t.worker_instance_id.as_str(), "w-1");
        assert_eq!(t.lease_ttl_ms, 30_000);
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
            flow_id: None,
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
    fn scanner_filter_noop_and_default() {
        let f = ScannerFilter::default();
        assert!(f.is_noop());
        assert_eq!(f, ScannerFilter::NOOP);
        // A no-op filter matches any candidate, including ones that
        // produced no HGET results.
        assert!(f.matches(None, None));
        assert!(f.matches(Some(&Namespace::new("t1")), Some("v")));
    }

    #[test]
    fn scanner_filter_namespace_match() {
        let f = ScannerFilter {
            namespace: Some(Namespace::new("tenant-a")),
            instance_tag: None,
        };
        assert!(!f.is_noop());
        assert!(f.matches(Some(&Namespace::new("tenant-a")), None));
        assert!(!f.matches(Some(&Namespace::new("tenant-b")), None));
        // Missing core namespace ⇒ no match.
        assert!(!f.matches(None, None));
    }

    #[test]
    fn scanner_filter_instance_tag_match() {
        let f = ScannerFilter {
            namespace: None,
            instance_tag: Some(("cairn.instance_id".into(), "i-1".into())),
        };
        assert!(f.matches(None, Some("i-1")));
        assert!(!f.matches(None, Some("i-2")));
        assert!(!f.matches(None, None));
    }

    #[test]
    fn scanner_filter_both_dimensions() {
        let f = ScannerFilter {
            namespace: Some(Namespace::new("tenant-a")),
            instance_tag: Some(("cairn.instance_id".into(), "i-1".into())),
        };
        assert!(f.matches(Some(&Namespace::new("tenant-a")), Some("i-1")));
        assert!(!f.matches(Some(&Namespace::new("tenant-a")), Some("i-2")));
        assert!(!f.matches(Some(&Namespace::new("tenant-b")), Some("i-1")));
        assert!(!f.matches(None, Some("i-1")));
    }

    #[test]
    fn scanner_filter_builder_construction() {
        // Simulates external-crate usage: only public constructors
        // and chainable setters (no struct-literal access to the
        // `#[non_exhaustive]` fields).
        let empty = ScannerFilter::new();
        assert!(empty.is_noop());
        assert_eq!(empty, ScannerFilter::NOOP);

        let ns_only = ScannerFilter::new().with_namespace(Namespace::new("tenant-a"));
        assert!(!ns_only.is_noop());
        assert!(ns_only.matches(Some(&Namespace::new("tenant-a")), None));

        let tag_only = ScannerFilter::new().with_instance_tag("cairn.instance_id", "i-1");
        assert!(!tag_only.is_noop());
        assert!(tag_only.matches(None, Some("i-1")));

        let both = ScannerFilter::new()
            .with_namespace(Namespace::new("tenant-a"))
            .with_instance_tag("cairn.instance_id", "i-1");
        assert!(both.matches(Some(&Namespace::new("tenant-a")), Some("i-1")));
        assert!(!both.matches(Some(&Namespace::new("tenant-b")), Some("i-1")));
    }

    // ── RFC-015 Stream-durability-mode types ──

    #[test]
    fn stream_mode_constructors_and_wire_str() {
        assert_eq!(StreamMode::durable().wire_str(), "durable");
        assert_eq!(StreamMode::durable(), StreamMode::Durable);

        let s = StreamMode::durable_summary();
        assert_eq!(s.wire_str(), "summary");
        match s {
            StreamMode::DurableSummary { patch_kind } => {
                assert_eq!(patch_kind, PatchKind::JsonMergePatch);
            }
            _ => panic!("expected DurableSummary"),
        }

        let b = StreamMode::best_effort_live(15_000);
        assert_eq!(b.wire_str(), "best_effort");
        match b {
            StreamMode::BestEffortLive { config } => {
                assert_eq!(config.ttl_ms, 15_000);
                assert_eq!(config.maxlen_floor, 64);
                assert_eq!(config.maxlen_ceiling, 16_384);
                assert!((config.ema_alpha - 0.2).abs() < 1e-9);
            }
            _ => panic!("expected BestEffortLive"),
        }

        let cfg = BestEffortLiveConfig::with_ttl(10_000)
            .with_maxlen_floor(128)
            .with_maxlen_ceiling(8_192)
            .with_ema_alpha(0.3);
        let b2 = StreamMode::best_effort_live_with_config(cfg);
        match b2 {
            StreamMode::BestEffortLive { config } => {
                assert_eq!(config.ttl_ms, 10_000);
                assert_eq!(config.maxlen_floor, 128);
                assert_eq!(config.maxlen_ceiling, 8_192);
                assert!((config.ema_alpha - 0.3).abs() < 1e-9);
            }
            _ => panic!("expected BestEffortLive"),
        }

        assert_eq!(StreamMode::default(), StreamMode::Durable);
    }

    #[test]
    fn tail_visibility_default_and_wire() {
        assert_eq!(TailVisibility::default(), TailVisibility::All);
        assert_eq!(TailVisibility::All.wire_str(), "");
        assert_eq!(
            TailVisibility::ExcludeBestEffort.wire_str(),
            "exclude_best_effort"
        );
    }

    #[test]
    fn append_frame_outcome_summary_version_builder() {
        let base = AppendFrameOutcome::new("1713-0", 3);
        assert_eq!(base.stream_id, "1713-0");
        assert_eq!(base.frame_count, 3);
        assert_eq!(base.summary_version, None);

        let with_v = AppendFrameOutcome::new("1713-0", 3).with_summary_version(7);
        assert_eq!(with_v.summary_version, Some(7));
        assert_eq!(with_v.clone(), with_v);
    }

    /// RFC-015 §3.2 null-sentinel round-trip invariant.
    ///
    /// The sentinel (`"__ff_null__"`) is the byte-exact string callers
    /// use to encode "set this leaf to JSON null" in a JSON Merge Patch
    /// frame (because RFC 7396 otherwise treats `null` as delete-key).
    /// The invariant: on read, the summary document NEVER contains the
    /// sentinel string — it is always rewritten to JSON null.
    ///
    /// The full end-to-end invariant is exercised against the Lua
    /// applier in `crates/ff-test/tests/engine_backend_stream_modes.rs`
    /// (integration). Here we just pin the byte sequence + the
    /// type-level constant so any accidental edit of the sentinel
    /// string fails the unit build before it reaches an integration
    /// run.
    #[test]
    fn summary_null_sentinel_byte_exact() {
        assert_eq!(SUMMARY_NULL_SENTINEL, "__ff_null__");
        assert_eq!(SUMMARY_NULL_SENTINEL.len(), 11);
        assert!(SUMMARY_NULL_SENTINEL.bytes().all(|b| b.is_ascii()));
        assert_eq!(SUMMARY_NULL_SENTINEL.trim(), SUMMARY_NULL_SENTINEL);
    }

    #[test]
    fn summary_document_constructor() {
        let doc = SummaryDocument::new(
            br#"{"tokens":3}"#.to_vec(),
            2,
            PatchKind::JsonMergePatch,
            1_700_000_100,
            1_700_000_000,
        );
        assert_eq!(doc.version, 2);
        assert_eq!(doc.patch_kind, PatchKind::JsonMergePatch);
        assert_eq!(doc.first_applied_ms, 1_700_000_000);
        assert_eq!(doc.clone(), doc);
    }

    #[test]
    fn frame_builder_sets_mode() {
        let f = Frame::new(b"p".to_vec(), FrameKind::Event);
        assert_eq!(f.mode, StreamMode::Durable);

        let f = Frame::new(b"p".to_vec(), FrameKind::Event)
            .with_mode(StreamMode::durable_summary());
        match f.mode {
            StreamMode::DurableSummary { patch_kind } => {
                assert_eq!(patch_kind, PatchKind::JsonMergePatch);
            }
            other => panic!("expected DurableSummary, got {other:?}"),
        }
    }

    #[test]
    fn backend_config_valkey_ctor() {
        let c = BackendConfig::valkey("host.local", 6379);
        // Wave 0 (#230) added `BackendConnection::Postgres`, so the
        // match is no longer irrefutable. `let-else` keeps the test
        // focused on the Valkey arm without a full `match`.
        let BackendConnection::Valkey(v) = &c.connection else {
            panic!("expected Valkey connection, got Postgres");
        };
        assert_eq!(v.host, "host.local");
        assert_eq!(v.port, 6379);
        assert!(!v.tls);
        assert!(!v.cluster);
        assert_eq!(c.timeouts, BackendTimeouts::default());
        assert_eq!(c.retry, BackendRetry::default());
        assert_eq!(c.clone(), c);
    }
}
