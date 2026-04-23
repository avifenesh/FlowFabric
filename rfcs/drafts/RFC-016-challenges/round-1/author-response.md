# RFC-016 Round 1 — Author response

Summary of dispositions (K=correctness, L=ergonomics, M=implementation):

| Dissent | Disposition | RFC change |
| --- | --- | --- |
| K.D1 LetRun + impossible Q3 overlap | CONCEDE | §5 paragraph on impossible-under-LetRun added |
| K.D2 / M.D5 replay persisted pending-cancel | CONCEDE | §8.5 rewrite + Invariant Q6 + Stage C reconciler |
| K.D3 AllOf post-impossible counters | CONCEDE | §3 step 2 clarification |
| K.D4 set_edge_group_policy ordering | CONCEDE, pick (a) | §6.1 tightened to "before first `add_dependency`" |
| K.D5 dynamic expansion k>n | CONCEDE | §8.4 tolerance made explicit + metric reason |
| L.D1 worked examples | CONCEDE | §2.1 added with both snippets |
| L.D2 dual entry point | CONCEDE, pick (a) | drop `add_dependency` sugar; §11 Stage B revised |
| L.D3 name-based lookup | CONCEDE, pick (b) | §6.2 note — name helpers are SDK, out of RFC |
| M.D1 AllOf group-hash overhead | CONCEDE, option (b) sparse fields | §3 + §6.3 sparse-fields note |
| M.D2 cancel batching mechanism | CONCEDE | §4.1 added; §11 Stage C gate benchmark |
| M.D3 policy_variant compactness | PARTIAL-CONCEDE | §6.3 keeps string for debuggability, adds explicit trade-off note + future-compaction hook |
| M.D4 metric label anti-goal | CONCEDE | §7 commitment added |

All K and L dissents conceded in full. M.D3 partial — author prefers readable string with explicit trade note, which addresses M's core concern (acknowledge the trade). All other M dissents conceded in full.

Revisions are applied in the next commit (`rfc(016): round-1 revisions`) and the next round of challengers evaluates the revised RFC.
