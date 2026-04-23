# Main pre-0.4.0 smoke (2026-04-23)

**Scratch:** `/tmp/ff-smoke-main-prepatch-2` (deleted after smoke).
**Main HEAD:** `ffa56bd` (post #145/#146/#147/#151/#152/#155/#156/#157).
**Exerciser:** Worker XXX-2 (retry after XXX API error).

## Default-features build

`cargo build` on the scratch crate (ff-sdk default = `valkey-default`
on): **green**, 1 downstream warning (`parse_success_result` dead-code
in ff-sdk, pre-existing). `cargo run` prints the expected
`pre-0.4.0 smoke compile ok`.

## --no-default-features build

Scratch crate declares its own `valkey` feature forwarding to
`ff-sdk/valkey-default` and sets `ff-sdk = { default-features = false }`.
`cargo build --no-default-features`: **green**. ff-sdk recompiled
without ferriskey-surface modules (`task`, `worker`, `snapshot`,
`admin`, `engine_error::enrich_dependency_conflict`). Consumer-side
code gated behind `#[cfg(feature = "valkey")]` drops cleanly — the
`FlowFabricWorker::connect_with` + `completion_backend()` exerciser
correctly vanishes when the feature is off. RFC-012 §1.3 agnosticism
claim holds for ff-sdk's own surface.

## New API surface exercised (list of 7)

1. **ScannerFilter DX re-export (#157).** `use ff_core::ScannerFilter;`
   + `ScannerFilter::new().with_namespace("tenant-a").with_instance_tag("k","v")`
   compiles. `with_namespace` accepts `&'static str` via
   `impl Into<Namespace>` — no `Namespace::new(...)` ceremony needed.
2. **BackendConfig re-export (#157).**
   `ff_backend_valkey::BackendConfig::valkey("127.0.0.1", 6379)` resolves
   via the `pub use ff_core::backend::BackendConfig;` line in
   ff-backend-valkey's lib root.
3. **BackendError seal (#151).** `SdkError::Backend(BackendError::Valkey { kind, .. })`
   matches; `kind.as_stable_str()` returns the lowercase wire string
   (`"transport" | "protocol" | "timeout" | "auth" | "cluster" |
   "busy_loading" | "script_not_loaded" | "other"`). **Note:**
   `BackendError` is an `enum` with a single `Valkey` variant today,
   not a struct — the task-sheet template
   `BackendError { kind, .. }` needs to be spelled
   `BackendError::Valkey { kind, .. }`. Doesn't change user-facing
   stability; just a spec-nit.
4. **UsageDimensions dedup\_key (#145).** Construction blocked at the
   idiomatic `UsageDimensions { dedup_key: ..., ..Default::default() }`
   form (E0639, see papercuts). Fallback `Default::default()` +
   field-set assignment compiles; fields are `pub` so this is a valid
   escape hatch.
5. **Frame extension (#147).** `Frame::new(vec![], FrameKind::Event)`
   compiles. `FrameKind` variants are `{Stdout, Stderr, Event, Blob}`
   — no `Delta` variant (the smoke-sheet's example name). SDK-side
   `"delta"` is encoded via `Frame::frame_type: String`, not the
   `FrameKind` enum. `Frame` is also `#[non_exhaustive]`, so
   consumer-side struct-literal construction is blocked — only the
   `Frame::new` / `with_seq` / `with_frame_type` / `with_correlation_id`
   builders are reachable. That's a **design choice**, not a papercut.
6. **`FlowFabricWorker::connect_with` signature.** Confirmed
   `(WorkerConfig, Arc<dyn EngineBackend>, Option<Arc<dyn CompletionBackend>>)
   -> Result<Self, SdkError>` (ff-sdk/src/worker.rs:572). Smoke
   compile passes type-checking.
7. **`completion_backend()` accessor.** Confirmed
   `-> Option<Arc<dyn ff_core::completion_backend::CompletionBackend>>`
   (ff-sdk/src/worker.rs:622). Option wrapper retained for the
   "backend doesn't support push-completion" shape.

## Regression vs v0.3.4

None found at the compile surface for the exercised APIs. All seven
touch-points compile both with and without `valkey-default`.
BackendError wire strings, `Frame` builders, and `connect_with` /
`completion_backend` signatures line up with the CHANGELOG
`[Unreleased]` entries (#155) and the cairn-migration guide (#152).

## Papercuts (non-blocker)

1. **UsageDimensions has no inherent constructor + is `#[non_exhaustive]`.**
   Downstream crates cannot write
   `UsageDimensions { dedup_key: Some("k".into()), ..Default::default() }`
   — the E0639 check rejects `..Default::default()` on
   `#[non_exhaustive]` structs from other crates. The only supported
   path today is `let mut u = UsageDimensions::default(); u.dedup_key =
   Some("k".into());`. Fine for ergonomics inside ff-sdk (same crate,
   rule doesn't apply); awkward for the 3rd-party / cairn-fabric SDK
   story. Compare `Frame::new(...)` + `.with_*` chain — that pattern
   works nicely and could be mirrored here as
   `UsageDimensions::new()` + `.with_dedup_key(..)` /
   `.with_input_tokens(..)`. **Size:** ~15 lines, no behavior change,
   adds a clear idiomatic path. Not blocking 0.4.0 but worth a
   post-tag polish issue.

2. **Cosmetic — smoke-sheet drift vs `BackendError` shape.** The
   pre-0.4.0 smoke template lists
   `SdkError::Backend(BackendError { kind, .. })`. Today's actual
   shape needs `BackendError::Valkey { kind, .. }`. Either the
   current enum should gain a non-variant `kind()` pattern (it already
   has the method), or the docs/template should be updated. Spec-nit,
   not an API change.

## Recommendation

**Green-light 0.4.0 tag pending Stage 1c T4** (Worker ZZ-10's seal of
`FlowFabricWorker::client()` + migration of stream fns). The new API
surface from #145/#146/#147/#151/#157 is coherent, compiles under both
feature configurations, and matches the CHANGELOG + migration
guide. The UsageDimensions papercut is worth filing as a follow-up
but does not block the tag — `Default` + field-set is a viable escape
hatch and the pattern is used sparingly (report\_usage call sites
only).
