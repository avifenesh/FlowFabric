#!/usr/bin/env bash
# Download / warm models used by the media-pipeline example:
#   - Qwen2.5-0.5B-Instruct Q4_K_M GGUF (summarize worker)
#   - fastembed MiniLM-L6-v2, warmed by running `embed --warm` (self-
#     verifies the fastembed cache — a pre-existing .onnx does not
#     guarantee the right model or a complete download).
#
# Idempotent: Qwen download skips when the file already exists AND passes
# the GGUF magic-byte check. fastembed warmup runs every invocation; it's
# sub-second once the cache is populated.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MODEL_DIR="$PKG_DIR/models"
mkdir -p "$MODEL_DIR"

# F10: preflight disk check — the Qwen download is ~350MB; we want a
# little headroom on top (model + .tmp during download + scratch).
MIN_FREE_KB=500000
FREE_KB="$(df -k "$MODEL_DIR" | awk 'NR==2 {print $4}')"
if [[ -z "${FREE_KB:-}" ]] || [[ "$FREE_KB" -lt "$MIN_FREE_KB" ]]; then
    echo "[error] need >=${MIN_FREE_KB}KB free in $MODEL_DIR (have ${FREE_KB:-unknown}KB)" >&2
    exit 1
fi

QWEN_NAME="qwen2.5-0.5b-instruct-q4_k_m.gguf"
QWEN_PATH="$MODEL_DIR/$QWEN_NAME"
QWEN_URL="https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/$QWEN_NAME"

# F9: GGUF magic-byte check. Every GGUF v1+ file begins with the ASCII
# bytes "GGUF". A partial download with an HTTP 200 (seen on HF CDN when
# a connection drops mid-stream) yields a truncated file that won't parse
# when llama-cpp-2 loads it; catch that here with a clear error.
#
# Limitations (R2-F34): this catches truncation + totally-wrong-format
# but NOT arbitrary mid-file corruption (bit flips, zero-fill). A full
# sha256 check would be stronger, but Hugging Face doesn't expose stable
# checksums per file revision. If we ever move to a registry that does
# (e.g. ollama / an S3 bucket with ETag), switch to sha256.
is_valid_gguf() {
    local f="$1"
    [[ -s "$f" ]] || return 1
    # Read first 4 bytes; compare to literal "GGUF".
    local head
    head="$(head -c 4 "$f" 2>/dev/null || true)"
    [[ "$head" == "GGUF" ]]
}

if [[ -f "$QWEN_PATH" ]] && is_valid_gguf "$QWEN_PATH"; then
    echo "[skip] Qwen GGUF already present and valid: $QWEN_PATH"
else
    if [[ -f "$QWEN_PATH" ]]; then
        echo "[warn] existing Qwen GGUF is corrupt or truncated — re-downloading"
        rm -f "$QWEN_PATH"
    fi
    echo "[get]  Qwen GGUF (~350MB) -> $QWEN_PATH"
    curl -L --fail --retry 3 --retry-delay 2 \
        -o "$QWEN_PATH.tmp" "$QWEN_URL"
    if ! is_valid_gguf "$QWEN_PATH.tmp"; then
        rm -f "$QWEN_PATH.tmp"
        echo "[error] downloaded Qwen GGUF failed magic-byte check (got 4-byte head != 'GGUF')" >&2
        exit 1
    fi
    mv "$QWEN_PATH.tmp" "$QWEN_PATH"
fi

# F8: warm the fastembed cache by actually running the embed binary with
# --warm. This self-verifies the cache — a pre-existing .onnx file from
# an unrelated fastembed run would pass `find` but might be the wrong
# model or a partial download. Running the real init + embed is the only
# reliable check. Requires the `embed` binary to have been built (e.g.
# `cargo build -p media-pipeline --bin embed`).
echo "[warm] fastembed MiniLM-L6-v2 via embed --warm"
# Debug build on purpose: a fresh checkout builds the whole example crate
# here, and --release adds 5–10 min for a one-time cache warmup run. The
# embed binary does fastembed::try_new + one trivial embed() call — debug
# build is plenty fast for that.
if ! cargo run --quiet --manifest-path "$PKG_DIR/Cargo.toml" --bin embed -- --warm; then
    echo "[error] fastembed warmup failed — check cargo/embed build + network" >&2
    exit 1
fi

echo
echo "Model inventory:"
du -h "$MODEL_DIR"/*.gguf 2>/dev/null || true
FASTEMBED_CACHE="${HOME}/.cache/fastembed"
if [[ -d "$FASTEMBED_CACHE" ]]; then
    du -sh "$FASTEMBED_CACHE" 2>/dev/null || true
fi
