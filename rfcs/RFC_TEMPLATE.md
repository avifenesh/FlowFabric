# RFC-NNN: Title

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md

---

## Summary

One paragraph: what this RFC defines and why it matters.

## Motivation

Which use cases from the pre-RFC drive this primitive? Reference UC-XX identifiers.

## Detailed Design

### Object Definition

What fields exist. Types. Required vs optional.

### Invariants

Numbered invariants with rationale. These are the non-negotiable correctness rules.

### State Transitions / Lifecycle

What states exist, what transitions are allowed, what is forbidden.

### Operations

Each operation: name, parameters, semantics, atomicity class, error cases.

### Valkey Data Model

How this maps to Valkey keys, data structures, and Lua scripts.
Which operations must be atomic (Class A) vs durable append (Class B) vs derived (Class C).

### Error Model

Explicit error types with when they occur.

## Interactions with Other Primitives

How this RFC relates to adjacent RFCs. Cross-references.

## V1 Scope

What is in v1, what is designed-for-but-deferred.

## Open Questions

Only genuinely unresolved questions. Do not reopen locked decisions from the pre-RFC.

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md
- Related RFCs: RFC-NNN, RFC-NNN
