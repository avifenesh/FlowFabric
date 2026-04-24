//! Consumer-facing import path for the waitpoint HMAC wire types and
//! rotation args.
//!
//! **v0.7 migration-master Q4.** Today (Valkey backend) all
//! signing and verification of waitpoint HMAC tokens happens
//! **server-side inside the FlowFabric Lua library** — the
//! `ff_sign_waitpoint_token` / `ff_validate_waitpoint_token` FCALLs
//! read the per-partition `waitpoint_hmac_secrets` hash under the
//! same redis.call("TIME") clock as suspension / signal delivery.
//! There is no pure-Rust sign or verify code in the workspace, and
//! the v0.7 Postgres backend (Wave 4) will continue the pattern via
//! stored procedures on the global `ff_waitpoint_hmac(kid, secret,
//! rotated_at)` table.
//!
//! This module exists so external crates have ONE stable path
//! (`ff_core::waitpoint_hmac`) to import the wire-token type, the
//! per-partition keystore snapshot shape, and the rotation Args /
//! Result shapes from. Re-exports from `ff_core::backend` and
//! `ff_core::contracts` — no new types live here and no logic runs
//! through this module.
//!
//! # Signing + verification location
//!
//! | Backend  | Where `sign` lives                                    | Where `verify` lives                                   |
//! |----------|-------------------------------------------------------|--------------------------------------------------------|
//! | Valkey   | `lua/waitpoint_hmac.lua` (FCALL `ff_sign_waitpoint_token`) | `lua/waitpoint_hmac.lua` (FCALL `ff_validate_waitpoint_token`) |
//! | Postgres | Wave-4 stored proc on `ff_waitpoint_hmac`             | Wave-4 stored proc on `ff_waitpoint_hmac`              |
//!
//! Consumers never touch the raw HMAC-SHA256 computation from Rust —
//! the backend owns the secret material, so the signing / verifying
//! code runs co-located with the key-storage primitive on each
//! backend.
//!
//! # Rotation
//!
//! See [`EngineBackend::rotate_waitpoint_hmac_secret_all`](crate::engine_backend::EngineBackend::rotate_waitpoint_hmac_secret_all)
//! for the cluster-wide rotation method added in v0.7. The existing
//! per-partition free-function helper
//! [`ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions`]
//! stays available for direct-Valkey consumers on older SDKs.

pub use crate::backend::WaitpointHmac;
pub use crate::contracts::{
    ListWaitpointHmacKidsArgs, RotateWaitpointHmacSecretAllArgs,
    RotateWaitpointHmacSecretAllEntry, RotateWaitpointHmacSecretAllResult,
    RotateWaitpointHmacSecretArgs, RotateWaitpointHmacSecretOutcome, VerifyingKid,
    WaitpointHmacKids,
};
