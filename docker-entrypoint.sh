#!/bin/sh
set -e

# Support both BACKEND_URLS and legacy OLLAMA_URLS
FINAL_BACKENDS="${BACKEND_URLS:-$OLLAMA_URLS}"
FINAL_BACKENDS="${FINAL_BACKENDS:-http://localhost:11434}"

PORT="${PORT:-11435}"
TIMEOUT="${TIMEOUT:-300}"
HOST="${HOST:-0.0.0.0}"

echo "Starting ollamaMQ with backends: $FINAL_BACKENDS"

exec /app/ollamaMQ --port "$PORT" --host "$HOST" --backend-urls "$FINAL_BACKENDS" --timeout "$TIMEOUT" "$@"
