#!/usr/bin/env bash
# Download / warm models used by the media-pipeline example:
#   - Qwen2.5-0.5B-Instruct Q4_K_M GGUF (summarize worker)
#   - fastembed MiniLM-L6-v2 is auto-downloaded by fastembed on first use;
#     we pre-warm the cache by calling the lib once via the `embed` binary.
#
# Idempotent: each step skips if the artifact is already present.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MODEL_DIR="$PKG_DIR/models"
mkdir -p "$MODEL_DIR"

QWEN_NAME="qwen2.5-0.5b-instruct-q4_k_m.gguf"
QWEN_PATH="$MODEL_DIR/$QWEN_NAME"
QWEN_URL="https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/$QWEN_NAME"

if [[ -f "$QWEN_PATH" ]]; then
    echo "[skip] Qwen GGUF already cached: $QWEN_PATH"
else
    echo "[get]  Qwen GGUF (~350MB) -> $QWEN_PATH"
    curl -L --fail --retry 3 --retry-delay 2 \
        -o "$QWEN_PATH.tmp" "$QWEN_URL"
    mv "$QWEN_PATH.tmp" "$QWEN_PATH"
fi

FASTEMBED_CACHE="${HOME}/.cache/fastembed"
if [[ -d "$FASTEMBED_CACHE" ]] && find "$FASTEMBED_CACHE" -type f -name '*.onnx' -print -quit | grep -q .; then
    echo "[skip] fastembed MiniLM already cached under $FASTEMBED_CACHE"
else
    echo "[note] fastembed MiniLM (~90MB) auto-downloads on first use of the embed binary."
    echo "       Run: cargo run -p media-pipeline --bin embed -- --warm"
fi

echo
echo "Model inventory:"
du -h "$MODEL_DIR"/*.gguf 2>/dev/null || true
if [[ -d "$FASTEMBED_CACHE" ]]; then
    du -sh "$FASTEMBED_CACHE" 2>/dev/null || true
fi
