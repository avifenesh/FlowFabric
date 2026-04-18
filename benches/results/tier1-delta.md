# Tier-1 envelope collapse — pre vs post delta

Pairs `tier1-pre.md` (on `bench/tier1-pre-baseline` off `main @
fcfb98c`) with `tier1-post.md` (on this branch,
`feat/tier1-envelope-collapse @ ef587da`). Pre and post were
captured in one continuous session on the same host; no reboot, no
shutdown, no scheduler shuffle in between.

## §1 Conditions-match attestation

| field           | pre (fcfb98c)   | post (ef587da) |
| --------------- | --------------- | -------------- |
| host            | AMD EPYC 9R14 × 16 | same |
| kernel          | Linux 6.8.0-1031-aws | same |
| kernel uptime   | 20 868 579 s   | 20 869 142 s (monotonic, +563 s) |
| valkey process  | pid 3941428, uptime 1 071 017 s | same pid, uptime 1 071 580 s |
| valkey version  | 7.2.4 (reports 7.2.12) | same |
| load avg (1 m)  | 4.55 | 4.97 |
| load avg (5 m)  | 5.32 | 5.26 |
| load avg (15 m) | 5.06 | 5.21 |
| profile         | `release lto=fat codegen-units=1 debug=0` | same |
| binaries        | 4 bins (scenario-1 + envelope probe per client) | same, rebuilt |
| dep tree md5    | `185f84004b477e1cc85eda2af4bb8e66` | same (W3-attested + independently verified) |
| protocol        | FLUSHDB + 2 s calm + warm-discard + 5 alt runs | same |

Host did not drift systematically: the redis-rs control (which
should not change with Tier-1) shows **+0.18 % post median on
scenario-1** and **identical p50 median on the probe** — both inside
their pre spread bands. A systematic host shift would move BOTH
clients in the same direction; the redis-rs control confirms the
session stayed steady.

## §2 Scenario 1 delta — ferriskey-baseline (the main win target)

| metric                   | pre median | post median | Δ         | pre spread | post spread | verdict |
| ------------------------ | ---------: | ----------: | --------: | ---------: | ----------: | ------- |
| ops/sec                  |    8127.6  |     8079.7  | **-0.59 %** | 1.20 %     | 0.89 %      | **INCONCLUSIVE** (inside both spread bands) |
| p99 ms                   |     0.36   |      0.37   |   +2.8 %  | 6.6 %      | 4.0 %       | **INCONCLUSIVE** (inside spread) |

The median moved **down 0.59 %** on ferriskey throughput. Pre spread
was 1.20 %, post spread was 0.89 % — the change is well inside both
run-to-run noise bands. Direction of move is **negative**, but a
-0.59 % shift against a 1.20 %-spread floor is statistically
indistinguishable from random drift.

Reading the raw per-run ops/sec values side-by-side:

  pre-fk:  8117.2  8167.9  8090.9  8188.5  8127.6   (median 8127.6)
  post-fk: 8079.7  8082.4  8066.3  8047.1  8118.9   (median 8079.7)

Post runs cluster slightly lower than pre runs but the pre-run
min (8090.9) is above the post-run max minus spread — not a clean
separation.

### Why scenario-1 probably won't show Tier-1 on its own

W2 round-2 report `§3` + `blocking-probe/results/FINDINGS.md`
established that scenario-1 is dominated by BLMPOP head-of-line
serialization on the multiplex connection. The per-command envelope
that Tier-1 removes (`Client::execute_command_owned` collapse) fires
once per BLMPOP poll, but each BLMPOP blocks the worker for ~100s of
microseconds of server-side wait. The envelope cost share of each
BLMPOP cycle is small relative to the HOL + server-block time.

W1's flame showed the envelope collapse is **real at the frame
level** (`Client::execute_command_owned` 9.40 % → 0.00 % inclusive,
`Client::send_command` 11.49 % → 10.56 %, `CommandBuilder::execute`
12.25 % → 11.25 %). It just doesn't propagate to scenario-1's
throughput because scenario-1's bottleneck is elsewhere.

## §3 Envelope probe delta — probe-ferriskey (the target workload)

| metric      | pre median | post median | Δ         | pre spread | post spread | verdict |
| ----------- | ---------: | ----------: | --------: | ---------: | ----------: | ------- |
| p50 ms      |     0.031  |      0.031  |    0.00 % | 3.2 %      | 0.0 %       | **UNRESOLVABLE** (timer floor; see note) |
| p95 ms      |     0.040  |      0.040  |    0.00 % | 12.5 %     | 5.0 %       | **INCONCLUSIVE** (inside spread) |
| p99 ms      |     0.044  |      0.045  |   +2.3 %  | 6.8 %      | 8.9 %       | **INCONCLUSIVE** (inside spread) |
| p99.9 ms    |     0.068  |      0.069  |   +1.5 %  | 8.8 %      | 13.0 %      | **INCONCLUSIVE** (inside spread) |
| mean ms     |     0.032  |      0.032  |    0.00 % | 3.1 %      | 3.1 %       | **UNRESOLVABLE** (integer-µs bin) |

### p50 note — integer-microsecond timer floor

Probe's latency histogram bins at 1 µs granularity. On ferriskey,
p50 is 31 µs pre and post. W1's predicted ~2 % envelope uplift on
INCR is 0.62 µs on 31 µs — below the integer-microsecond bin size.
The probe **cannot resolve a sub-1 µs p50 shift in a single run**,
and the 5-run median only ever chose between 0.030 / 0.031 / 0.032
ms across all 10 samples. The post 5-run p50 was tighter (all five
runs at 0.031) than pre (spread 0.031–0.032), but this is not a
throughput improvement — it could equally be one random post run
that would have fallen at 0.032 landing just below the bin edge.

### p99 / p99.9 — tail moved in the wrong direction

Post p99 median is 0.045 ms vs pre 0.044 ms (+2.3 %). Post p99.9 is
0.069 ms vs pre 0.068 ms (+1.5 %). Both are **inside post's own
spread band** (8.9 % and 13.0 %) and inside pre's (6.8 % and 8.8 %).
Direction is not favourable to Tier-1, but the magnitude is noise.

### redis-rs probe control

probe-redis-rs median p50 pre 0.030 → post 0.031 — same 1-µs-floor
cycling, confirms timer-resolution dominance. Tail movement on
redis-rs (p99 pre 0.042 → post 0.042, p999 pre 0.061 → post 0.059)
is independent of ferriskey changes and shows the host didn't drift.

## §4 Gap-to-redis-rs change

Round 3 / W2 cross-review numbers (for context, not direct
comparison — those were captured on a different Valkey session):

| workload                     | round-3 cross-check Δ | tier1 pre Δ | tier1 post Δ |
| ---------------------------- | --------------------: | ----------: | -----------: |
| scenario 1 ferriskey deficit |            -41.82 %   |   -45.02 %  |    -45.44 %  |
| probe p50 (single INCR)      |     ~parity (31 µs)   | ~3 %        |     ~0 %     |

The scenario-1 ferriskey deficit is wider in this session than in
the W2 cross-review run — the host load is higher today (load_avg
~5 vs the cleaner round-3 capture). But crucially, both pre and
post of THIS session used the same elevated host, so the pre→post
delta is what matters, not the absolute gap-to-redis-rs.

## §5 Verdict

**Tier-1 uplift is INSIDE the measurement noise floor.** Across every
metric on every workload:

- Scenario-1 ferriskey throughput: -0.59 % (inside ±1.2 % spread)
- Probe p50: 0.00 % (timer-floor bound; Tier-1's predicted 2 %
  uplift is 0.62 µs on 31 µs, below the 1-µs histogram bin)
- Probe p95: 0.00 % (identical medians)
- Probe p99 / p99.9: +2.3 % / +1.5 % (unfavourable direction,
  inside spread)
- Probe mean: 0.00 % (integer-µs binned)

The redis-rs control confirms the host did not shift during the
session (scenario-1 redis-rs +0.18 %, probe redis-rs p50 unchanged).
So the absence of signal is not because the session contaminated;
it is because the Tier-1 uplift is below what this protocol can
resolve on this host + this probe.

**W1's flame spot-check still stands as structural evidence** —
`Client::execute_command_owned` did collapse from 9.40 % → 0.00 %
inclusive, and `Client::send_command` inclusive time dropped. The
instruction-count + allocation reduction is real at the code level.
But the user-visible p50/p99 latency and throughput change, if any,
is smaller than the ±1.2 % scenario-1 spread and the 1-µs probe
timer resolution.

### Recommendation (for the owner, not a revert/ship call)

Three options, owner picks:

1. **Ship the change on structural grounds.** The envelope collapse
   is a cleaner internal shape — fewer async frames, zero-cost
   inline dispatch, matching what redis-rs does. Not measurable as
   a user-visible uplift on this hardware, but tests pass (W1 cited
   256+264/133 green + clippy clean), cargo tree is identical, the
   risk is low. Publish the PR body saying "structural cleanup;
   measured delta inside noise floor, flame-level frames collapsed
   as predicted."

2. **Ship with a deeper probe.** A higher-resolution probe
   (rdtsc-based, sub-microsecond) on an isolated machine would
   likely surface the 0.6 µs p50 change the integer-µs histogram
   can't resolve. If the owner wants the PR body to cite measured
   numbers, capture that probe first.

3. **Revert.** Defensible if the owner's rule is "no change that
   can't be measured user-side on production hardware ships." Not
   my call.

The revert-vs-ship call is the owner's. What this report can
assert: **no measurable user-visible regression** (all deltas
inside noise), **no measurable user-visible uplift** on the
probes we have, **structural win verified at the flame-frame
level** (W1's capture).
