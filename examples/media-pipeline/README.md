# Media Pipeline

A three-stage audio processing pipeline on FlowFabric: **transcribe → summarize → embed**, with a human-in-the-loop approval gate between summarize and embed.

This example exercises three mechanisms shipped in Batch B:

- **Capability routing** (RFC-009): each execution declares `required_capabilities`; each worker advertises its `capabilities`. The scheduler subset-matches so transcribe tasks never reach the embed worker, etc. Executions that cannot be matched to any capable worker block in `lane_blocked_route` and are promoted by the unblock scanner when a capable worker appears.
- **Stream tail with terminal signal** (RFC-006): workers stream incremental output (transcribe lines, LLM tokens, embed completion) as frames; the submit and review CLIs tail concurrently and stop cleanly on the stream's `closed_at` / `closed_reason` terminal markers instead of timeout fallbacks.
- **HMAC-signed waitpoint signals** (RFC-004): the summarize worker suspends and receives a `waitpoint_token`. The review CLI fetches that token from `GET /v1/executions/{id}/pending-waitpoints` and must include it in the resume signal. Tampering with the token (via `--tamper-token`) surfaces as a server-side `invalid_token` rejection.

## Architecture

```
submit CLI             FlowFabric Server          Workers
    |                        |                       |
    |-- POST /flows -------->|                       |
    |-- POST /executions --->|                       |
    |   (caps=asr)           |<-- claim (caps=asr) --| transcribe-worker
    |                        |<-- append_frame ------|   (whisper.cpp stdout)
    |<-- tail_stream --------|<-- complete ----------|
    |-- GET  /result ------->|                       |
    |-- POST /executions --->|                       |
    |   (caps=llm, payload=T)|<-- claim (caps=llm) --| summarize-worker
    |                        |<-- append_frame ------|   (qwen2.5-0.5b tokens)
    |                        |<-- suspend + wpt ----|
    |<-- tail_stream --------|                       |
    |                        |                       |
  review CLI                 |                       |
    |-- GET  /state -------->|                       |
    |-- GET  /pending-wp --->|                       |
    |-- POST /signal+token ->|                       |
    |                        |-- resume ------------>| (same worker re-claims)
    |                        |<-- complete ----------|
submit CLI                   |                       |
    |-- POST /executions --->|                       |
    |   (caps=embed)         |<-- claim (caps=embed)-| embed-worker
    |                        |<-- complete ----------|
    |<-- tail_stream --------|   all three streams closed_at set
    |  exit 0
```

## Prerequisites

- Valkey running on `localhost:6379`
- FlowFabric server running on `localhost:9090` with `FF_WAITPOINT_HMAC_SECRET` set **and `FF_LANES=default,media`** — see [Server lane requirement](#server-lane-requirement) below.
- `cmake`, `git`, `make`, `espeak-ng`, `ffmpeg` on `$PATH` (used by the setup and sample-generation scripts)

### Server lane requirement

The engine's unblock scanner only polls lane-scoped blocked-index sets for the lanes the server was started with. This pipeline uses the `media` lane exclusively, so a server started with the default `FF_LANES=default` will leave `media`-lane executions stuck in `blocked_by_route` forever even when a capable worker connects. Start the server with both lanes:

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
FF_LANES=default,media \
    cargo run -p ff-server --release
```

(The HMAC secret is required for the suspend-and-resume path — the server refuses to boot without it.)

## First-time setup

```bash
# From examples/media-pipeline/

scripts/setup.sh               # clones + builds whisper.cpp, fetches ggml-tiny.en
scripts/download-models.sh     # pulls qwen2.5-0.5b-instruct + pre-warms minilm
scripts/generate-sample.sh     # writes samples/{story,tech,conversation}.wav
```

Each script is idempotent — re-running only does missing work.

## Running the demo

```bash
# From examples/media-pipeline/
scripts/demo.sh
```

That spawns the three workers in the background, runs `submit`, runs `review --auto-approve`, and exits 0 on success. Per-worker logs land in `target/demo-logs/`.

For an interactive walkthrough, run the pieces by hand:

```bash
# Terminal A — start the three workers
cargo run --bin transcribe &
cargo run --bin summarize &
cargo run --bin embed &

# Terminal B — submit (samples: story.wav, tech.wav, or conversation.wav)
cargo run --bin submit -- --audio samples/story.wav --title demo

# Terminal C — as soon as submit prints "[submit] summarize=<uuid>",
# start the reviewer against that UUID:
cargo run --bin review -- --execution-id <uuid>
```

The review CLI prints incoming `summary_token` frames while waiting for the suspend, then prompts `Approve? [y/n]:`.

## Expected wall-time (CPU only)

Qwen2.5-0.5B-Instruct Q4_K_M runs the full 200-token summary in about **10 minutes on a 16-core EPYC** — roughly 3.4 s per sampled token. Transcribe (whisper.cpp tiny.en-q5_1) and embed (MiniLM-L6-v2) each finish in under a second. A whole pipeline run on `samples/tech.wav` wall-clocks at ~12 minutes end-to-end.

The bottleneck is the `llama-cpp-2 = "0.1", default-features = false` choice in `Cargo.toml`, which builds llama.cpp for stock CPU (no OpenMP, no offload). For realtime-feeling runs, flip on a hardware backend:

```toml
# GPU / accelerator options (pick one appropriate for your box)
llama-cpp-2 = { version = "0.1", default-features = false, features = ["cuda"] }     # Nvidia
llama-cpp-2 = { version = "0.1", default-features = false, features = ["metal"] }    # Apple Silicon
llama-cpp-2 = { version = "0.1", default-features = false, features = ["vulkan"] }   # portable
```

Each feature pulls the corresponding backend in `llama-cpp-sys-2`; see `cargo info llama-cpp-2` for the authoritative feature list.

## Golden smoke output

A trimmed reference run is checked in at [`docs/golden-smoke.txt`](docs/golden-smoke.txt). It captures the `[submit]` driver lines, the `[review]` auto-approve handshake, the first 20 streamed `summary_token` frames, the persisted `summary_final` frame, the 384-dim `EmbedResult` metadata, and the `WAITPOINT_TOKEN=...` disclosure line the review CLI parses.

## Validation scenarios

### 1. Capability routing — block + unblock

Kill the embed worker before submitting, watch the pipeline block at the embed stage, then restart it and watch the unblock scanner promote the execution.

```bash
# With only transcribe + summarize running:
cargo run --bin submit -- --audio samples/story.wav &
cargo run --bin review -- --execution-id <uuid> --auto-approve

# submit output stalls after "summarize done" because embed has no claimer.
# Start the embed worker now:
cargo run --bin embed

# Within seconds, submit prints "embed done" and exits 0.
```

### 2. HMAC enforcement — tampered token

```bash
cargo run --bin review -- \
    --execution-id <summarize-uuid> \
    --tamper-token \
    --auto-approve
```

The review CLI flips one hex char of the token before sending. The server rejects with `400 Bad Request: ff_deliver_signal failed: invalid_token` and the review CLI exits 0 (the rejection is the expected behavior). The summarize execution remains suspended; a second clean `review` call can approve it.

### 3. Concurrent tail cap

```bash
# Set the server's max concurrent stream ops low, then fire several tails.
FF_MAX_CONCURRENT_STREAM_OPS=2 cargo run -p ff-server &

# In another shell, tail the same stream five times in parallel.
# Three of the five get HTTP 429 while the cap is saturated.
```

## V1 shortcuts (documented)

- **Client-side data passing (legacy, to be migrated).** The submit CLI currently waits for each upstream execution to complete, reads the raw result bytes from `GET /v1/executions/{id}/result`, and embeds them as the next execution's `input_payload`. Batch C item 3 has landed server-side auto-injection: when the flow edge is staged with a non-empty `data_passing_ref`, the engine atomically copies the upstream's `result` into the downstream's `input_payload` at satisfaction time inside `ff_resolve_dependency`. The submit CLI has not yet been migrated to the server-side path — tracked as a follow-up. See `docs/rfc011-operator-runbook.md` §"Data passing between flow nodes".
- **`direct-valkey-claim` feature.** Workers use the direct Valkey claim path gated by the `direct-valkey-claim` feature on `ff-sdk`. Batch C will replace this with a proper scheduler-mediated claim API.

## Submit crash recovery (v1)

Because submit in its current form orchestrates the 3-stage sequence client-side, **a crash or Ctrl-C between stages leaves the flow stuck**: transcribe + summarize may exist as members with edges applied, but embed was never POSTed. Once the submit CLI is migrated to stage all 3 executions up-front with `data_passing_ref` edges (follow-up to Batch C item 3), the server drives the pipeline forward autonomously and a crash becomes cosmetic tail-drop instead of a stuck flow.

Recovery options:

1. **Cancel the stuck flow.** Easiest. Find the flow_id in submit's log output and:

   ```bash
   curl -X POST http://localhost:9090/v1/flows/<flow-id>/cancel \
       -H 'content-type: application/json' \
       -d '{"flow_id":"<flow-id>","reason":"submit_crashed","cancellation_policy":"cancel_all","now":<ms>}'
   ```

   The pipeline is lost but downstream consumers see a terminal cancelled state.

2. **Finish manually.** Read each staged execution's state and result, and POST the next stage yourself via the same REST calls submit uses. Non-trivial — recommend option 1 unless the transcribe / summarize result is expensive to rerun.

3. **Migrate submit to the server-driven chain** (Batch C item 3 has landed the engine side). Post all three executions up-front with `data_passing_ref` set on the edges; the engine auto-injects upstream payloads into downstream `input_payload` as each stage completes. Submit becomes a tail multiplexer and a crash is cosmetic tail-drop, not a stuck flow.

## File layout

```
examples/media-pipeline/
    Cargo.toml                 # workspace = []; one crate, five bins, one lib
    src/lib.rs                 # canonical shared types (PipelineInput,
                               # TranscribeResult, SummarizeResult,
                               # EmbedResult, ApprovalDecision)
    src/bin/
        transcribe.rs          # W1 — whisper.cpp, caps=asr
        summarize.rs           # W3 — qwen2.5, caps=llm, suspends for review
        embed.rs               # W3 — minilm, caps=embed
        submit.rs              # W2 — flow orchestrator + tail multiplexer
        review.rs              # W2 — approval signal with HMAC token
    scripts/
        setup.sh               # whisper.cpp + ggml model
        download-models.sh     # qwen GGUF + minilm warmup
        generate-sample.sh     # three canned WAVs via espeak-ng + ffmpeg
        demo.sh                # one-liner end-to-end demo
```
