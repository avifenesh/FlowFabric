//! Shared types for the deploy-approval UC-27/21 example.
//!
//! Pipeline:
//!   submit
//!     └─► build (DurableSummary stream of log lines — RFC-015)
//!           └─► [unit, integration, e2e]  (AllOf — RFC-007)
//!                 └─► deploy (canary + suspend Count{n=2, DistinctSources} — RFC-013/014)
//!                       └─► verify (on --fail → cancel_flow cascade — RFC-016)
//!
//! Capability routing (RFC-009): each test execution advertises one of
//! `kind=unit|integration|e2e`; the worker that matches claims it.

use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER_URL: &str = "http://localhost:9090";
pub const DEFAULT_NAMESPACE: &str = "deploy-approval";

pub const LANE_BUILD: &str = "build";
pub const LANE_TEST: &str = "test";
pub const LANE_DEPLOY: &str = "deploy";
pub const LANE_VERIFY: &str = "verify";

pub const EXECUTION_KIND_BUILD: &str = "deploy_build";
pub const EXECUTION_KIND_TEST: &str = "deploy_test";
pub const EXECUTION_KIND_DEPLOY: &str = "deploy_rollout";
pub const EXECUTION_KIND_VERIFY: &str = "deploy_verify";

pub const CAP_ROLE_BUILD: &str = "role=build";
pub const CAP_ROLE_DEPLOY: &str = "role=deploy";
pub const CAP_ROLE_VERIFY: &str = "role=verify";

pub const CAP_TEST_UNIT: &str = "kind=unit";
pub const CAP_TEST_INTEGRATION: &str = "kind=integration";
pub const CAP_TEST_E2E: &str = "kind=e2e";

pub const SIGNAL_NAME_APPROVAL: &str = "deploy_approval";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPayload {
    pub artifact: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestPayload {
    pub kind: String,
    pub artifact: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployPayload {
    pub artifact: String,
    pub approval_waitpoint_key: String,
    pub canary_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPayload {
    pub artifact: String,
    pub flow_id: String,
    /// When true the verify worker reports failure and triggers
    /// `cancel_flow { CancelAll }` on its own flow — demonstrating the
    /// RFC-016 cascade path for a bad deploy.
    pub fail: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPayload {
    pub approved: bool,
    pub reviewer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    pub artifact: String,
    pub replicas: u32,
}
