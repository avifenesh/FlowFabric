//! SQLite dialect-forked query modules per RFC-023 §4.1.
//!
//! PG migrations and runtime queries cannot be shared with SQLite —
//! the dialect gap (partitioning, `RETURNING` subsets, `jsonb`, etc.)
//! requires a parallel module tree. This tree mirrors
//! `ff-backend-postgres` one-for-one so parity review is a straight
//! file-level diff.
//!
//! Phase 2a.1 establishes the module layout. Method bodies land in
//! Phase 2a.2 (attempt / exec_core) and Phase 2a.3 (lease / dispatch).

pub mod attempt;
pub mod dispatch;
pub mod exec_core;
pub mod flow;
pub mod flow_staging;
pub mod lease;
pub mod signal;
pub mod stream;
pub mod suspend;
pub mod waitpoint;
