# Review W3 — Real-life example validation

Branch: `feat/ferriskey-iam-gate` (HEAD `205f242`)
Date: 2026-04-18. Worker-3 round-3 review.

## Scope

Validate that the two FlowFabric examples continue to build, test, and
run end-to-end against the ferriskey + ff-sdk + ff-server changes on
this branch:

1. `examples/coding-agent` — LLM-powered coding agent with HMAC
   suspend+resume review flow.
2. `examples/media-pipeline` — transcribe → summarize → embed pipeline
   exercising capability routing, stream tail, and HMAC signal.

Per manager's post-initial-findings directive, this review also
covered fixing defects surfaced during validation, whether introduced
on this branch or latent on `main`.

## Final verdict

**GREEN — examples work, merge is safe.**

| Example        | Build                  | Unit tests       | Smoke (e2e)                     | Verdict |
|----------------|------------------------|------------------|---------------------------------|---------|
| coding-agent   | GREEN (release, 1m19s) | 0/0/0 (no tests) | LLM path skipped (no key)       | GREEN   |
| media-pipeline | GREEN after 3 fixes    | 0/0/0 (no tests) | `demo.sh` PASS, 5m04s wall      | GREEN   |

Three defects were found and fixed on this branch as part of the
validation:

1. **`io-std` feature propagation** — latent; was masked on `main` by
   telemetrylib's transitive dep chain. Fixed in `0cb1fb3`.
2. **`demo.sh` review race** — pre-existing on both branches. Fixed
   in `205f242`.
3. **embed tail-join hang** — pre-existing on both branches. Fixed
   in `205f242`.

None of the three was a ferriskey API regression. #1 was a direct
consequence of a legitimate ferriskey simplification on this branch
(removal of `telemetrylib` in `2a36b46`) exposing a missing feature
declaration in `media-pipeline/Cargo.toml`. #2 and #3 were independent
example-code defects that had been shipping broken on `main` and were
uncovered while validating the ferriskey changes.

## Fix 1 — `io-std` feature declaration (`0cb1fb3`)

**Symptom:** `cargo build --release -p media-pipeline` on the branch
fails on the `submit` bin:
```
error[E0432]: unresolved import `tokio::io::Stdout`
error[E0425]: cannot find function `stdout` in module `tokio::io`
note: the item is gated behind the `io-std` feature
```

**Affected site:** `examples/media-pipeline/src/bin/submit.rs:85`
```rust
let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
```

**Cargo.toml tokio features** (pre-fix):
```toml
features = ["rt-multi-thread", "macros", "time", "process",
            "io-util", "signal", "fs", "sync"]
```
No `io-std`.

**Why this only broke on the branch:** On `main`, `cargo tree -i
tokio -e features` shows:
```
tokio v1.52.1
└── hyper-util
    └── reqwest
        └── opentelemetry-otlp
            └── telemetrylib
                └── ferriskey (feature "default")
```
`opentelemetry-otlp` and `reqwest` transitively demand tokio with
`io-std`. Commit `2a36b46` on this branch dropped `telemetrylib`
entirely and replaced its OpenTelemetry bridge with `tracing` events;
the transitive enablement went with it. Same `submit.rs` source on
both branches — only the feature resolution differs.

**Fix:** One-line addition to tokio's features array. The example's
`Cargo.toml` should have declared `io-std` explicitly all along; it
was relying on an opaque transitive chain.

**Evidence of green state after fix:** `cargo build --release -p
media-pipeline` completes cleanly in 22s (incremental), 5 bins
produced (`transcribe`, `summarize`, `embed`, `submit`, `review`).

## Fix 2 — `demo.sh` review race (`205f242`)

**Symptom:** `./scripts/demo.sh samples/story.wav` aborts in ~1s with:
```
Error: get execution failed: 404 Not Found — review needs the
current attempt index
[demo] tearing down
```

**Root cause:** `submit.rs:107-109` (pre-fix) logged all three EIDs
at the top of `run_pipeline` — before any execution existed on the
server:
```rust
log(stdout, &format!("flow_id={flow_id}")).await;
log(stdout, &format!("transcribe={eid_transcribe}")).await;
log(stdout, &format!("summarize={eid_summarize}")).await;
log(stdout, &format!("embed={eid_embed}")).await;
```

`demo.sh:70-76` greps `[submit] summarize=` and launches
`review --execution-id $EID` as soon as that line is seen. `review`'s
`fetch_current_attempt_index` (`review.rs:438+`) calls
`GET /v1/executions/{id}` with no retry on 404 — by design, to
detect server version skew — and exits. Since `submit` only creates
the summarize execution after transcribe completes
(`submit.rs:220+`), there's always a 404 when `review` starts.

**Verified pre-existing on `main`:** Fresh clone at
`/tmp/mp-main-probe` — identical code at `submit.rs:107-109` and
`demo.sh:70-76`. Not a branch-introduced regression.

**Fix:** Move the `summarize={eid}` log line to just after
`create_execution(eid_summarize, ...)` + `add_to_flow`. Same for
`embed={eid}`. `flow_id` and `transcribe={eid}` stay at the top for
early-failure diagnostics.

## Fix 3 — embed tail-join hang (`205f242`)

**Symptom:** After `[submit] embed done — pipeline complete` is
printed, the submit process never exits. `demo.sh` then hangs on
`wait "$SUBMIT_PID"` indefinitely. Wall time for a ~3-minute pipeline
balloons past 15 minutes with no further progress and no error.

**Root cause:** `submit.rs:322` (pre-fix) did:
```rust
let _ = tokio::join!(tail_transcribe, tail_summarize, tail_embed);
```
The tail loop (`submit.rs:565+`) exits when the server reports
`closed_at` on the attempt stream. But `embed.rs:171` calls
`task.complete(Some(result))` without writing any stream frames —
the attempt stream key `ff:stream:{p:N}:{eid}:0` is never created in
Valkey, and the tail endpoint returns `{"frames":[], "count":0}`
(verified with `curl` during the hang). No `closed_at` ever arrives;
the tail loops forever.

**Verified pre-existing on `main`:** same submit.rs code and same
behavior on `main`. Branch-independent.

**Fix:** Replace the happy-path `tokio::join!` with three
`drain_and_stop(...).await` calls, reusing the same 300ms-grace /
abort pattern already in place on the error paths. Buffered frames
still flush; executions that never wrote a stream no longer block
shutdown.

**Cross-reference:** The upstream question — should ff-server or the
SDK emit a sentinel `closed_at` even when the attempt stream has zero
frames? — is a fair follow-up, but the per-caller bounded drain is
the right defensive posture regardless. Not filed as a separate
issue; called out here for whoever owns ff-server.

## Cap-match pubsub — ruled out (diagnostic)

An earlier attempt to reproduce the demo returned a misleading
"transcribe worker connected with matching caps but never claims"
symptom. Root cause was operator error: the prior ff-server instance
had been started with `FF_LANES=default,media` but my restart used
the default `FF_LANES=default`. The engine's unblock scanner only
polls lane-scoped blocked-index sets for configured lanes, so the
transcribe execution in `ff:idx:{p:120}:lane:media:blocked:route`
was never drained.

This is **documented** in `examples/media-pipeline/README.md:45-54`:
```
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) FF_LANES=default,media
```

Once the server was restarted with the correct lane config, the
transcribe worker claimed its execution in under 1s and processed
it in 869ms. Both branches (feat/ferriskey-iam-gate HEAD and main
HEAD) reproduce the success path identically when configured
correctly.

Per manager directive, this was cross-verified on main:
- Main pipeline also reaches "embed done — pipeline complete".
- Main's demo.sh also fails on review race and submit tail-join.
- No cap-match-pubsub regression exists on either branch.

## Happy-path smoke summary

End-to-end `./scripts/demo.sh samples/story.wav` on this branch after
all three fixes:

```
06:31:45  demo.sh started
06:31:47  transcribe worker connected (caps: asr, whisper-tiny-en)
06:31:47  summarize worker connected (caps: llm, qwen-500m-q4)
06:31:48  embed worker connected (caps: embed, minilm-l6)
...
06:36:48  [review] execution suspended — fetching waitpoint token
06:36:48  [review] signal delivered: effect=resume_condition_satisfied
06:36:48  [submit] summarize done (50 tokens, 231 chars)
06:36:49  [submit] embed done — pipeline complete
06:36:49  [demo] PASS — pipeline completed
06:36:49  demo.sh exited
                                               wall: 5m04s
```

Architectural mechanisms exercised and confirmed working:
- **Capability routing** (Scenario 5 equivalent): transcribe, summarize,
  embed workers each claim only their matching-cap execution; each
  correctly rejected wrong-cap executions as inline-claim mismatch
  (verified in worker logs).
- **Stream tail** (Scenario 4 equivalent): transcribe → summarize
  streaming path produces character-level progress, summary_final
  frame arrives, `[summarize] stream closed` observed.
- **HMAC suspend + resume** (Scenario 2 equivalent): summarize emits
  `REVIEW_NEEDED` + `WAITPOINT_TOKEN=k1:...`, review bin fetches
  pending waitpoints and delivers the signal, server responds with
  `effect=resume_condition_satisfied`, summarize resumes and
  completes from the persisted `summary_final` frame.

The ferriskey API-surface changes in this branch (IAM feature gate,
blocking-cmd timeout configurability, ff-sdk/ff-server feature
propagation) pass through cleanly to the examples. No lazy_connect
deprecation warnings, no Telemetry-stub imports, no unresolved
re-exports from ff-sdk.

## coding-agent — build + unit tests only

`cargo build --release -p coding-agent-example` finished in 1m19s on
this branch. All 3 bins built (`submit`, `worker`, `approve`).
Warnings: dead-code only, all pre-existing in the example itself
(e.g. unused `AgentStep` struct, unused `DEFAULT_MODEL` const). None
reference ferriskey / ff-sdk / ff-server.

`cargo test --release -p coding-agent-example` ran 0 tests across
the 3 bins (no `#[test]` functions in the example). Exit 0.

End-to-end LLM smoke was skipped per manager directive —
`OPENROUTER_API_KEY` unset in this environment, cost + rate-limit
exposure outweighed the incremental confidence. The HMAC
suspend+resume mechanism coding-agent uses is the same mechanism
exercised end-to-end in the media-pipeline smoke above, via
`review --auto-approve` against `ff_deliver_signal`. Same API,
same wire format, already green.

## Appendix A — commands run

```
# Build green, both examples
(cwd) examples/coding-agent
cargo build --release                              # 1m19s
cargo test --release                               # 0 tests, exit 0

(cwd) examples/media-pipeline
cargo build --release                              # 22s after io-std fix
cargo test --release --lib --bin transcribe \
        --bin summarize --bin embed --bin review    # 0 tests, exit 0

# End-to-end smoke
(cwd) /home/ubuntu/FlowFabric
kill $(pgrep -f "./target/release/ff-server") && \
valkey-cli -p 6379 FLUSHALL && \
env FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
    FF_LANES=default,media \
    ./target/release/ff-server &

(cwd) examples/media-pipeline
WHISPER_CLI=$PWD/vendor/whisper.cpp/build/bin/whisper-cli \
WHISPER_MODEL=$PWD/models/ggml-tiny.en-q5_1.bin \
QWEN_MODEL=$PWD/models/qwen2.5-0.5b-instruct-q4_k_m.gguf \
    ./scripts/demo.sh                              # PASS at 5m04s

# Main-branch comparison
(cwd) /home/ubuntu/FlowFabric
git checkout main
cargo build --release -p ff-server
(cd examples/media-pipeline && cargo build --release)
# Same lane/HMAC env + workers + submit →
# submit.log: "[submit] embed done — pipeline complete"
# then 10+ min tail-join hang (pre-existing on main, now fixed on branch)
```

## Appendix B — environmental state

```
valkey-cli PING                                    # PONG (localhost:6379)
ff-server                                          # compiled from this
                                                   # branch HEAD, launched
                                                   # with FF_LANES=
                                                   # default,media + fresh
                                                   # FF_WAITPOINT_HMAC_SECRET
OPENROUTER_API_KEY                                 # UNSET
examples/media-pipeline/models/                    # whisper ggml + Qwen
                                                   # GGUF present
examples/media-pipeline/vendor/whisper.cpp/        # whisper-cli built
~/.cache/fastembed                                 # populated during smoke
                                                   # (~90MB MiniLM download)
examples/media-pipeline/samples/                   # story.wav, tech.wav,
                                                   # conversation.wav
```

## Appendix C — commits landed on this branch in round-3 review

- `0cb1fb3` — `examples/media-pipeline: declare tokio io-std feature explicitly`
- `205f242` — `examples/media-pipeline: fix demo.sh review race + embed tail-join hang`

Both are example-scope only; no ferriskey / ff-sdk / ff-server source
touched.
