# RFC-016 Round 3 — K (correctness)

**Verdict:** ACCEPT

## Round-2 disposition review

| Round-2 dissent | Resolved? |
| --- | --- |
| K.D6 batch disposition contract + Lua error semantics | YES — §4.1 dispatch contract paragraph covers per-id disposition, ack policy for `cancelled`/`already_terminal`, `not_found` retention, and atomic Lua abort |
| K.D7 reconciler scan cost | YES — §6.3 index SET + §8.5 item 2 rewrite explicitly removes full-hash-scan; §11 Stage C aligned |

Both narrow round-2 correctness dissents closed.

## Per-section sweep of the revised RFC

- §2 / §2.1: GREEN
- §3 state machine + invariants Q1–Q6: GREEN — one-shot semantics, counter non-one-shot, sparse storage all internally consistent
- §4 CancelRemaining: GREEN
- §4.1 batching + dispatch contract + coalescing: GREEN — atomicity, disposition list, and coalescing all load-bearing behaviors are specified
- §5 LetRun + impossible-under-LetRun: GREEN
- §6 API + wire + forward-compat: GREEN
- §7 observability: GREEN
- §8 all sub-sections: GREEN (incl. §8.4 dynamic-expansion tolerance and §8.5 replay with index SET)
- §9 open questions: GREEN (remaining items are genuine future-consumer-feedback questions, not specification holes)
- §10 alternatives rejected: GREEN
- §11 staging + Stage C gates: GREEN — benchmark commitments are concrete and ship-blocking

No residual correctness concerns. ACCEPT.
