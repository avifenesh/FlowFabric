# RFC drafts — exploration record

This directory holds RFC drafts that were explored but **not merged as RFCs**. They are preserved as an exploration-and-reasoning record, not as authoritative design documents. Do not cite them as policy.

Each cluster of files represents one exploration. Conventions:

- `RFC-<NNN>-amendment-<topic>.md` — the draft under revision.
- `RFC-<NNN>-amendment-<topic>.<letter>-challenge.md` — a named challenger's adversarial review.

## Index

### RFC-012 namespace amendment (explored 2026-04-22, not pursued)

Tracks issue [#122](https://github.com/avifenesh/FlowFabric/issues/122) (cairn's multi-tenant isolation ask).

- `RFC-012-amendment-namespace.md` — draft v5 of a backend-level namespace-prefix amendment.
- `RFC-012-amendment-namespace.{K,L,M,P}-challenge.md` — four adversarial review rounds.

**Outcome:** the amendment grew to ~500 lines for one cairn request. After round-4 (Worker P) surfaced that `Namespace` already existed in `ff-core::types`, a peer-team dialog with cairn (in the #122 thread) right-sized the fix to a `ScannerFilter { namespace, instance_tag }` construction-time filter — no key-shape change, no RFC amendment. Shipped as PR [#127](https://github.com/avifenesh/FlowFabric/pull/127).

The draft is kept for three reasons:

1. Chain-of-reasoning for why the full prefix amendment was wrong-sized.
2. Record of the four-round challenger discipline pattern (K → L → M → P), reusable for future high-impact amendments.
3. Reference for if operator-level isolation (distinct from cairn's tenant/instance filter) becomes needed later.
