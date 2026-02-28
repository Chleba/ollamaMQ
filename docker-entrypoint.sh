#!/bin/sh
set -e

OLLAMA_URL="${OLLAMA_URL:-http://localhost:11434}"
PORT="${PORT:-11435}"

exec /app/ollamaMQ --port "$PORT" --ollama-url "$OLLAMA_URL" "$@"
