# Blocking-probe findings — ext sweep, 5x1 BLMPOP

Date: 2026-04-18. Worker-3, Commit 4 of ferriskey IAM-gate task.

## Setup

- 5 cloned clients × 1 concurrent BLMPOP each
- List pre-seeded with 2 elements; 2 tasks pop fast, 3 should block
- Sweep: `blocking_cmd_timeout_extension` ∈ {500ms, 1s, 2s} × `server_timeout` ∈ {5s, 2s}
- Two extra diagnostic runs: ext=10s and ext=20s at srv=5s

## Results (ext, srv_timeout) → (array, nil, err, max_wall_ms)

| ext     | srv | array | nil | err | max_wall | notes                              |
|---------|-----|-------|-----|-----|----------|------------------------------------|
| 500ms   | 5s  |   2   |  1  |  2  |  5501    | baseline — 2 of 3 blockers fail    |
| 500ms   | 2s  |   2   |  1  |  2  |  2501    | same shape at 2s                   |
| 1s      | 5s  |   2   |  1  |  2  |  6001    | ext=1s does NOT save responders    |
| 1s      | 2s  |   2   |  1  |  2  |  3001    | same                               |
| 2s      | 5s  |   2   |  1  |  2  |  7001    | ext=2s does NOT save responders    |
| 2s      | 2s  |   2   |  1  |  2  |  4001    | same                               |
| 10s     | 5s  |   2   |  2  |  1  | 15001    | 2nd blocker drains at T=10s        |
| 20s     | 5s  |   2   |  3  |  0  | 15081    | all 3 drain: T=5s, T=10s, T=15s    |

(`array` counts the fast-returners with a value; `other-ok` in the raw
logs — 2 per run — is counted as `array` here. `n_array=0` in the JSON
reflects a shape-classification quirk; the task-level log shows the two
fast returners.)

## Interpretation — this is NOT a safety-margin problem

If the issue were purely "client deadline is too tight", then
increasing the extension would recover late responders one by one.
It doesn't. The 1s and 2s runs show the exact same miss shape as 500ms:
the first blocker succeeds at T=server_timeout; the next two fail at
T=server_timeout+ext, regardless of whether ext is 500ms, 1s, or 2s.

The ext=10s and ext=20s runs reveal what's actually happening: the 2nd
blocker drains at T=10s (srv_timeout × 2) and the 3rd at T=15s
(srv_timeout × 3). Replies are being **serialized**, not lost.

This is the classic head-of-line blocking signature of a multiplexed
connection sender: blocking commands are dispatched on a single
inflight slot and the server cannot respond to the 2nd BLMPOP until
the 1st one's reply has cleared the wire. Each additional blocker
adds one full `server_timeout` of wall time.

### Retraction / refinement of W1 round-2 "extension too tight" framing

W1's round-2 report §2 characterised the issue as "500ms extension is
too tight — 5-concurrent BLMPOP at 5s server timeout: 2/3 late
responders lose replies." That framing would predict that bumping the
extension recovers the lost responders.

The sweep above falsifies that prediction: 1s and 2s extensions show
the exact same 2-err miss shape as 500ms. The 10s/20s diagnostic runs
then reveal the serial-drain signature (T=5s, 10s, 15s) that pins the
cause to mux head-of-line blocking, not the safety margin.

The HOL mechanism was already correctly identified in W1 round-2 §3
("p50-tied-tail-diverges" / candidate #2): the tail divergence is
each additional blocker queueing behind the previous one's response
slot. This findings doc refines §2 in light of the empirical sweep —
the §3 mechanism is the real one, and the §2 knob would not have
helped.

## What this means for the default

The extension knob can't paper over the multiplex serialization. To
"fix" all 5 concurrent blockers with the current architecture we'd
need `ext >= server_timeout × (N_blockers - 1)` — which defeats the
purpose of a safety margin (it would also delay legitimate deadline
errors for non-blocking commands by many seconds).

**Recommendation: DO NOT change the default from 500ms.** The real fix
is architectural — blocking commands need their own connection slot
(per-blocker dedicated connection, or a pool sized to expected
blocker concurrency) rather than sharing the mux. That's an RFC-sized
change, not a knob tweak.

The new `ClientBuilder::blocking_cmd_timeout_extension(Duration)` API
is still valuable: it lets operators who understand the tradeoff bump
the margin for their own workload shape (e.g. known-safe single-blocker
paths). But the default of 500ms is the right floor for typical
uses — making it larger by default would delay error surfacing on
genuinely stuck commands without solving the underlying problem.

## Raw data

Per-run JSON files are in this directory.

## Follow-up

Filed as GitHub issue #12 (labels: rfc, performance):
"ferriskey: dedicated connection for blocking commands — mux
head-of-line serialization". Tracks the architectural fix out of this
PR's scope.
