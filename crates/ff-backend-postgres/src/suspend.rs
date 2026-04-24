//! Suspend + resume plumbing for the Postgres backend.
//!
//! **Wave 4 Agent D (RFC-v0.7 v0.7 migration-master).**
//!
//! This module is the landing pad for the 4 suspend-family trait
//! methods (`suspend`, `observe_signals`, `list_suspended`,
//! `claim_resumed_execution`). The current tranche ships the
//! composite-condition evaluator as a pure-Rust helper ([`evaluate`])
//! and the parititon-slot helper ([`slot_of`]); the SQL bodies for
//! the trait methods land together with the Wave-4 claim/lease
//! plumbing being built in parallel (Wave 4 Agents A/B/C).
//!
//! # Composite evaluator placement
//!
//! Q11 pinned composite-condition evaluation at one of the three
//! SERIALIZABLE sites; we run the evaluator Rust-side inside the
//! SERIALIZABLE transaction (not as a plpgsql stored proc) because:
//!
//! 1. The declarative `ResumeCondition` + `CompositeBody` types
//!    already live in `ff-core::contracts`; replicating the logic in
//!    plpgsql would double-maintain a non-trivial tree walker.
//! 2. SERIALIZABLE already serializes reads + writes on the
//!    `satisfied_set` / `member_map` JSONB columns the evaluator
//!    touches, so moving the eval into pg buys no isolation.
//! 3. Rust-side eval keeps the evaluator unit-testable without a
//!    live Postgres (see [`evaluate`] tests below).
//!
//! The Valkey backend runs the equivalent logic in Lua; this is the
//! only evaluator divergence and is deliberate per the rationale
//! above.

use std::collections::{HashMap, HashSet};

use ff_core::backend::ResumeSignal;
use ff_core::contracts::{CompositeBody, CountKind, ResumeCondition, SignalMatcher};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::ExecutionId;

/// Map from `waitpoint_key` → list of signals delivered to it. This is
/// the shape [`ff_suspension_current.member_map`] deserialises into on
/// the Postgres side — signals are keyed by their target waitpoint,
/// so the evaluator can answer "did this waitpoint fire?" without a
/// per-signal wp lookup.
pub type SignalsByWaitpoint<'a> = HashMap<&'a str, &'a [ResumeSignal]>;

/// Map an `ExecutionId` to its partition slot under the deployment's
/// [`PartitionConfig`]. The `i16` return matches the column type on
/// every partitioned table in `migrations/0001_initial.sql`.
pub fn slot_of(exec_id: &ExecutionId, _cfg: &PartitionConfig) -> i16 {
    // `ExecutionId::partition()` is infallible on any validly-minted
    // id (it reads the `{fp:N}` hash-tag prefix). The partition
    // number is a u16 but our schema uses smallint (i16), which
    // accommodates the full 256-partition range with room to spare.
    exec_id.partition() as i16
}

/// Convert a `PartitionKey` to its slot index for schema-partition
/// keying. Returns `None` if the key is malformed.
pub fn slot_of_partition_key(pk: &PartitionKey) -> Option<i16> {
    pk.as_partition().ok().map(|p| p.index as i16)
}

/// Evaluate a [`ResumeCondition`] against the set of signals
/// delivered so far. Returns `true` iff the condition is satisfied.
///
/// `signals` is the full ordered list of [`ResumeSignal`]s recorded
/// against this suspension's `member_map`. The function walks the
/// declarative tree; it does NOT mutate inputs. Caller is responsible
/// for reading the satisfied-signals view from
/// `ff_suspension_current.member_map` under a SERIALIZABLE txn.
///
/// # Semantics (RFC-013 §2.4 + RFC-014 §2.1)
///
/// * `Single { waitpoint_key, matcher }` — `true` iff at least one
///   signal targeting `waitpoint_key` matches `matcher`.
/// * `OperatorOnly` — `false` (only an explicit operator resume
///   satisfies this condition; not reachable through this evaluator).
/// * `TimeoutOnly` — `false` (timeout-only suspensions resolve via
///   `timeout_behavior` at `timeout_at`, not via signals).
/// * `Composite(AllOf { members })` — `true` iff every `member`
///   evaluates `true`.
/// * `Composite(Count { n, count_kind, matcher, waitpoints })` —
///   `true` iff at least `n` distinct satisfiers (by `count_kind`)
///   match across signals targeting `waitpoints` and (optionally)
///   matching `matcher`.
pub fn evaluate(condition: &ResumeCondition, by_wp: &SignalsByWaitpoint<'_>) -> bool {
    match condition {
        ResumeCondition::Single {
            waitpoint_key,
            matcher,
        } => by_wp
            .get(waitpoint_key.as_str())
            .map(|sigs| sigs.iter().any(|s| matcher_matches(matcher, s)))
            .unwrap_or(false),
        ResumeCondition::OperatorOnly | ResumeCondition::TimeoutOnly => false,
        ResumeCondition::Composite(body) => evaluate_composite(body, by_wp),
        _ => false,
    }
}

fn evaluate_composite(body: &CompositeBody, by_wp: &SignalsByWaitpoint<'_>) -> bool {
    match body {
        CompositeBody::AllOf { members } => {
            !members.is_empty() && members.iter().all(|m| evaluate(m, by_wp))
        }
        CompositeBody::Count {
            n,
            count_kind,
            matcher,
            waitpoints,
        } => {
            // Gather all signals whose waitpoint is in `waitpoints` and
            // (optionally) match `matcher`. Collect alongside their
            // wp-key so `DistinctWaitpoints` can count distinct wp sets.
            let mut candidates: Vec<(&str, &ResumeSignal)> = Vec::new();
            for wpk in waitpoints {
                let Some(sigs) = by_wp.get(wpk.as_str()) else {
                    continue;
                };
                for s in sigs.iter() {
                    if matcher
                        .as_ref()
                        .map(|m| matcher_matches(m, s))
                        .unwrap_or(true)
                    {
                        candidates.push((wpk.as_str(), s));
                    }
                }
            }

            let distinct_count = match count_kind {
                CountKind::DistinctWaitpoints => {
                    let mut set: HashSet<&str> = HashSet::new();
                    for (wpk, _) in &candidates {
                        set.insert(wpk);
                    }
                    set.len() as u32
                }
                CountKind::DistinctSignals => {
                    let mut set: HashSet<String> = HashSet::new();
                    for (_, s) in &candidates {
                        set.insert(s.signal_id.0.to_string());
                    }
                    set.len() as u32
                }
                CountKind::DistinctSources => {
                    let mut set: HashSet<(&str, &str)> = HashSet::new();
                    for (_, s) in &candidates {
                        set.insert((s.source_type.as_str(), s.source_identity.as_str()));
                    }
                    set.len() as u32
                }
                // Forward-compat: unknown `CountKind` variants count
                // nothing (never satisfies) so a pinned evaluator
                // cannot silently resume under a future wire shape.
                _ => 0,
            };
            distinct_count >= *n
        }
        // Forward-compat: unknown `CompositeBody` variants never
        // satisfy. Same rationale as the `CountKind` arm.
        _ => false,
    }
}

fn matcher_matches(matcher: &SignalMatcher, signal: &ResumeSignal) -> bool {
    match matcher {
        SignalMatcher::ByName(name) => signal.signal_name.as_str() == name.as_str(),
        SignalMatcher::Wildcard => true,
        _ => false,
    }
}

#[cfg(test)]
mod evaluator_tests {
    use super::*;
    use ff_core::contracts::{CompositeBody, CountKind, ResumeCondition, SignalMatcher};
    use ff_core::types::{SignalId, TimestampMs};

    fn sig(wp_key: &str, name: &str, source_type: &str, source_identity: &str) -> ResumeSignal {
        // wp_key is not on ResumeSignal but used by the caller's map
        let _ = wp_key;
        ResumeSignal {
            signal_id: SignalId::new(),
            signal_name: name.to_owned(),
            signal_category: "external".to_owned(),
            source_type: source_type.to_owned(),
            source_identity: source_identity.to_owned(),
            correlation_id: String::new(),
            accepted_at: TimestampMs(0),
            payload: None,
        }
    }

    #[test]
    fn single_byname_matches() {
        let s = sig("wpk:a", "ready", "worker", "w1");
        let v = vec![s];
        let by_wp: SignalsByWaitpoint<'_> = [("wpk:a", v.as_slice())].into_iter().collect();
        let cond = ResumeCondition::Single {
            waitpoint_key: "wpk:a".into(),
            matcher: SignalMatcher::ByName("ready".into()),
        };
        assert!(evaluate(&cond, &by_wp));
    }

    #[test]
    fn single_byname_rejects_wrong_name() {
        let s = sig("wpk:a", "other", "worker", "w1");
        let v = vec![s];
        let by_wp: SignalsByWaitpoint<'_> = [("wpk:a", v.as_slice())].into_iter().collect();
        let cond = ResumeCondition::Single {
            waitpoint_key: "wpk:a".into(),
            matcher: SignalMatcher::ByName("ready".into()),
        };
        assert!(!evaluate(&cond, &by_wp));
    }

    #[test]
    fn count_distinct_sources_requires_two_distinct() {
        let dup_source = vec![
            sig("wpk:a", "x", "worker", "w1"),
            sig("wpk:a", "x", "worker", "w1"),
        ];
        let by_wp: SignalsByWaitpoint<'_> =
            [("wpk:a", dup_source.as_slice())].into_iter().collect();
        let cond = ResumeCondition::Composite(CompositeBody::Count {
            n: 2,
            count_kind: CountKind::DistinctSources,
            matcher: None,
            waitpoints: vec!["wpk:a".into()],
        });
        // Same source fires twice — must NOT satisfy.
        assert!(!evaluate(&cond, &by_wp));

        let distinct_sources = vec![
            sig("wpk:a", "x", "worker", "w1"),
            sig("wpk:a", "x", "worker", "w2"),
        ];
        let by_wp_ok: SignalsByWaitpoint<'_> =
            [("wpk:a", distinct_sources.as_slice())].into_iter().collect();
        assert!(evaluate(&cond, &by_wp_ok));
    }

    #[test]
    fn allof_two_waitpoints() {
        let a = sig("wpk:a", "x", "src", "id");
        let b = sig("wpk:b", "x", "src", "id");
        let va = vec![a];
        let vb = vec![b];
        let by_wp: SignalsByWaitpoint<'_> =
            [("wpk:a", va.as_slice()), ("wpk:b", vb.as_slice())]
                .into_iter()
                .collect();
        let cond = ResumeCondition::all_of_waitpoints(["wpk:a", "wpk:b"]);
        assert!(evaluate(&cond, &by_wp));

        // Missing wpk:b ⇒ not satisfied
        let only_a: SignalsByWaitpoint<'_> =
            [("wpk:a", va.as_slice())].into_iter().collect();
        assert!(!evaluate(&cond, &only_a));
    }
}
