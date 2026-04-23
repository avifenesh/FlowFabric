//! Sentry error-reporting integration (feature `sentry`).
//!
//! Opt-in error reporting for both engine-side (`ff-server`, `ff-engine`)
//! and consumer-side processes. Compiled out entirely when the `sentry`
//! feature is off; when on, exposes a single [`init_sentry`] entry point
//! that wires the Sentry client + a `sentry-tracing` layer into the
//! tracing subscriber installed by the caller.
//!
//! # Env-var contract
//!
//! All configuration is via `FF_SENTRY_*` env vars. No other code path
//! sets Sentry state, so a consumer that does not set `FF_SENTRY_DSN`
//! gets a zero-runtime-cost no-op — no network, no background thread.
//!
//! | Variable               | Required | Default           | Purpose                                                                   |
//! | ---------------------- | -------- | ----------------- | ------------------------------------------------------------------------- |
//! | `FF_SENTRY_DSN`        | yes      | —                 | Sentry project DSN. If unset, [`init_sentry`] returns `None` (no-op).     |
//! | `FF_SENTRY_ENVIRONMENT`| no       | `"production"`    | Sentry `environment` tag. Set to `staging`, `dev`, etc.                   |
//! | `FF_SENTRY_RELEASE`    | no       | crate `CARGO_PKG_VERSION` | Sentry `release` tag. Override to wire in a git SHA or build ID. |
//!
//! # Usage
//!
//! Call [`init_sentry`] **before** installing the tracing subscriber,
//! then compose [`tracing_layer`] into the subscriber so
//! `tracing::error!` events become Sentry events:
//!
//! ```ignore
//! use tracing_subscriber::prelude::*;
//!
//! let _sentry_guard = ff_observability::init_sentry();
//! tracing_subscriber::registry()
//!     .with(tracing_subscriber::fmt::layer())
//!     .with(tracing_subscriber::EnvFilter::from_default_env())
//!     .with(ff_observability::sentry_tracing_layer())
//!     .init();
//! // `_sentry_guard` must live until shutdown; dropping it flushes
//! // buffered events.
//! ```
//!
//! The returned [`sentry::ClientInitGuard`] must be held for the process
//! lifetime — dropping it early flushes and disables reporting.

/// Initialize Sentry from `FF_SENTRY_*` env vars.
///
/// Returns `Some(guard)` when `FF_SENTRY_DSN` is set and non-empty;
/// returns `None` when the DSN is unset or empty (graceful no-op).
/// Callers should hold the `Option` for the process lifetime regardless
/// — dropping `Some(guard)` early flushes and disables reporting.
///
/// This function only installs the Sentry **client**; it does NOT touch
/// the tracing subscriber. To bridge `tracing::error!` events into
/// Sentry, compose [`tracing_layer`] into your subscriber (see module
/// docs).
///
/// # Env vars honored
///
/// - `FF_SENTRY_DSN` — required; absence disables.
/// - `FF_SENTRY_ENVIRONMENT` — optional, default `"production"`.
/// - `FF_SENTRY_RELEASE` — optional, default this crate's version.
#[must_use = "drop the guard at shutdown to flush buffered events"]
pub fn init_sentry() -> Option<sentry::ClientInitGuard> {
    let dsn = std::env::var("FF_SENTRY_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;

    let environment = std::env::var("FF_SENTRY_ENVIRONMENT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "production".to_string());

    let release = std::env::var("FF_SENTRY_RELEASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let guard = sentry::init((
        dsn,
        sentry::ClientOptions {
            release: Some(release.into()),
            environment: Some(environment.into()),
            ..Default::default()
        },
    ));

    Some(guard)
}

/// Returns a `sentry-tracing` layer for composing into a
/// `tracing_subscriber` registry. Compose this *after* calling
/// [`init_sentry`]; events recorded before the client is initialized
/// are dropped.
///
/// The layer is cheap to construct even when no DSN is set — Sentry's
/// hub short-circuits when the client is a no-op.
pub fn tracing_layer<S>() -> sentry_tracing::SentryLayer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry_tracing::layer()
}

