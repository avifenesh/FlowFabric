//! Backend-shared capability matching predicate (RFC-v0.7 Q7).
//!
//! **Why this module exists.** `ff-backend-valkey` and `ff-backend-postgres`
//! both need to answer: "does worker W satisfy execution E's
//! `required_capabilities`?" In Valkey that's a Rust short-circuit in the
//! scheduler + authoritative Lua subset-check in `ff_issue_claim_grant`.
//! In Postgres it's a `WHERE required_caps <@ worker_caps` GIN query.
//! The *predicate* is identical across backends; only the storage
//! (CSV field vs `text[]`) differs.
//!
//! This module owns the pure-Rust predicate so both backends share one
//! definition + one test suite. The Valkey-side Lua (`lua/scheduling.lua`
//! `parse_capability_csv` + `missing_capabilities`) remains the atomic
//! authority inside the FCALL — this is a fast-path short-circuit.
//!
//! **Callers today:**
//!   - `ff-scheduler::claim` — Rust short-circuit before quota admission.
//!   - `ff-backend-postgres` (future) — direct predicate in the admission
//!     SQL / in-process query layer.
//!
//! **Wire shape.** `required_capabilities` is stored as a comma-separated
//! CSV on the Valkey contract layer (`RoutingRequirements::required_capabilities`
//! is serialized to CSV when written to the execution hash). Postgres will
//! store as `text[]`. This module accepts both:
//!   - [`matches_csv`] — CSV-form required set (existing call site).
//!   - [`matches`] — structured [`CapabilityRequirement`] (future call sites).
//!
//! The predicate semantics are **case-sensitive subset**: every non-empty
//! token in the required set must appear verbatim in the worker's
//! [`CapabilitySet`]. An empty required set trivially matches any worker
//! (backwards-compat default; see `RoutingRequirements` rustdoc).
//!
//! Bound constants ([`crate::policy::CAPS_MAX_BYTES`],
//! [`crate::policy::CAPS_MAX_TOKENS`]) live in `ff-core::policy` and are
//! enforced at ingress — not here. This module is a pure predicate.

use crate::backend::CapabilitySet;
use std::collections::BTreeSet;

/// A pre-parsed capability requirement (the "execution requires X" shape).
///
/// Thin newtype over a sorted token set — sorted so CSV serialization is
/// deterministic and log correlation is stable. The Valkey wire form is
/// the CSV join of this set; the Postgres wire form is the `text[]`
/// projection of `tokens`.
///
/// Constructed via [`CapabilityRequirement::new`] or parsed from CSV via
/// [`CapabilityRequirement::from_csv`]. The struct is `#[non_exhaustive]`
/// — additions (e.g. semver predicates, tier hints) land additively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapabilityRequirement {
    /// Required tokens. Empty → match any worker.
    pub tokens: BTreeSet<String>,
}

impl CapabilityRequirement {
    /// Build from any iterable of string-like tokens.
    ///
    /// Empty strings are dropped (they can't satisfy anything and would
    /// pollute the CSV form). Duplicates collapse via the `BTreeSet`.
    pub fn new<I, S>(tokens: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            tokens: tokens
                .into_iter()
                .map(Into::into)
                .filter(|t| !t.is_empty())
                .collect(),
        }
    }

    /// Parse from the Valkey wire form: comma-separated tokens.
    ///
    /// Mirrors Lua `parse_capability_csv` in `lua/scheduling.lua`:
    /// empty tokens (from `",gpu,,cuda,"`) are dropped. No validation
    /// of token contents — that's ingress's job (see
    /// `ff-scheduler::claim` and `ff-sdk::FlowFabricWorker::connect`).
    pub fn from_csv(csv: &str) -> Self {
        Self::new(csv.split(',').filter(|t| !t.is_empty()))
    }

    /// True iff no tokens are required (matches any worker).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Subset predicate: every required token appears in the worker's set.
///
/// Case-sensitive, exact-string match. Empty `required` → `true`. This
/// is the pure-Rust mirror of Lua `missing_capabilities` in
/// `lua/scheduling.lua`.
///
/// Called from:
///   - `ff-scheduler::claim` (fast-path short-circuit before quota admission)
///   - `ff-backend-postgres` (future; direct predicate in admission path)
pub fn matches(required: &CapabilityRequirement, worker: &CapabilitySet) -> bool {
    if required.is_empty() {
        return true;
    }
    // `CapabilitySet` is `Vec<String>` today; linear scan is fine for the
    // bounded sizes (CAPS_MAX_TOKENS = 256). If that changes, promote to
    // a HashSet lookup here.
    let worker_tokens: &[String] = &worker.tokens;
    required
        .tokens
        .iter()
        .all(|t| worker_tokens.iter().any(|w| w == t))
}

/// CSV-form subset predicate. Used by `ff-scheduler::claim` so the HGET
/// result can be fed in directly without parsing allocation.
///
/// Semantics identical to [`matches`]: every non-empty comma-separated
/// token in `required_csv` must appear in `worker_caps`. Empty or
/// all-separator CSV → `true`.
///
/// Kept as a separate entry point (rather than routing through
/// [`CapabilityRequirement::from_csv`] + [`matches`]) to avoid the
/// `BTreeSet` allocation on the scheduler hot path — the current call
/// site already has a `&BTreeSet<String>` of worker caps in hand.
pub fn matches_csv(required_csv: &str, worker_caps: &BTreeSet<String>) -> bool {
    required_csv
        .split(',')
        .filter(|t| !t.is_empty())
        .all(|t| worker_caps.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── matches_csv (scheduler hot path) ──

    #[test]
    fn empty_required_csv_matches_any_worker() {
        let worker: BTreeSet<String> = BTreeSet::new();
        assert!(matches_csv("", &worker));
        assert!(matches_csv("", &BTreeSet::from(["gpu".to_owned()])));
    }

    #[test]
    fn all_separator_csv_matches_any_worker() {
        // Regression: ",,," must still parse as empty-required.
        let worker = BTreeSet::from(["gpu".to_owned()]);
        assert!(matches_csv(",,,", &worker));
    }

    #[test]
    fn exact_match_csv() {
        let worker = BTreeSet::from(["gpu".to_owned(), "cuda".to_owned()]);
        assert!(matches_csv("gpu,cuda", &worker));
    }

    #[test]
    fn subset_match_csv() {
        let worker = BTreeSet::from([
            "gpu".to_owned(),
            "cuda".to_owned(),
            "fp16".to_owned(),
        ]);
        assert!(matches_csv("gpu,cuda", &worker));
        assert!(matches_csv("gpu", &worker));
    }

    #[test]
    fn missing_token_rejects_csv() {
        let worker = BTreeSet::from(["gpu".to_owned()]);
        assert!(!matches_csv("gpu,cuda", &worker));
        assert!(!matches_csv("cuda", &worker));
    }

    #[test]
    fn case_sensitive_csv() {
        // Documented: matching is case-sensitive. "GPU" ≠ "gpu".
        // Ingress is expected to normalize if callers want case-insensitive.
        let worker = BTreeSet::from(["gpu".to_owned()]);
        assert!(!matches_csv("GPU", &worker));
        assert!(matches_csv("gpu", &worker));
    }

    // ── matches (structured API) ──

    #[test]
    fn structured_empty_required_matches_any() {
        let req = CapabilityRequirement::default();
        let worker = CapabilitySet::default();
        assert!(matches(&req, &worker));
        assert!(matches(&req, &CapabilitySet::new(["gpu"])));
    }

    #[test]
    fn structured_subset_match() {
        let req = CapabilityRequirement::new(["gpu", "cuda"]);
        let worker = CapabilitySet::new(["gpu", "cuda", "fp16"]);
        assert!(matches(&req, &worker));
    }

    #[test]
    fn structured_missing_token_rejects() {
        let req = CapabilityRequirement::new(["gpu", "cuda"]);
        let worker = CapabilitySet::new(["gpu"]);
        assert!(!matches(&req, &worker));
    }

    #[test]
    fn structured_case_sensitive() {
        let req = CapabilityRequirement::new(["GPU"]);
        let worker = CapabilitySet::new(["gpu"]);
        assert!(!matches(&req, &worker));
    }

    #[test]
    fn from_csv_drops_empty_tokens() {
        let req = CapabilityRequirement::from_csv(",gpu,,cuda,");
        assert_eq!(req.tokens.len(), 2);
        assert!(req.tokens.contains("gpu"));
        assert!(req.tokens.contains("cuda"));
    }

    #[test]
    fn from_csv_empty_string_is_empty_requirement() {
        let req = CapabilityRequirement::from_csv("");
        assert!(req.is_empty());
    }

    // ── cross-API equivalence ──

    #[test]
    fn matches_and_matches_csv_agree() {
        // Same logical input via both entry points must give same answer.
        let cases = [
            ("", vec!["gpu"], true),
            ("gpu", vec!["gpu"], true),
            ("gpu,cuda", vec!["gpu"], false),
            ("gpu,cuda", vec!["gpu", "cuda", "fp16"], true),
            (",gpu,", vec!["gpu"], true),
            ("GPU", vec!["gpu"], false),
        ];
        for (req_csv, worker_tokens, expected) in cases {
            let worker_btree: BTreeSet<String> =
                worker_tokens.iter().map(|s| (*s).to_owned()).collect();
            let worker_set = CapabilitySet::new(worker_tokens.iter().copied());
            let req = CapabilityRequirement::from_csv(req_csv);

            assert_eq!(
                matches_csv(req_csv, &worker_btree),
                expected,
                "matches_csv({req_csv:?}) mismatch"
            );
            assert_eq!(
                matches(&req, &worker_set),
                expected,
                "matches({req_csv:?}) mismatch"
            );
        }
    }
}
