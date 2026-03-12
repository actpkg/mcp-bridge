#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPONENT_DIR="$(dirname "$SCRIPT_DIR")"
ACT="${ACT:-act}"
WASM="${COMPONENT_WASM:-$COMPONENT_DIR/target/wasm32-wasip2/release/mcp_bridge.wasm}"

if [ ! -f "$WASM" ]; then
  echo "WASM not found: $WASM"
  echo "Build first: cargo build --release (in $COMPONENT_DIR)"
  exit 1
fi

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null || echo 3456)

"$ACT" serve "$WASM" --listen "[::1]:$PORT" &
HOST_PID=$!
trap "kill $HOST_PID 2>/dev/null; wait $HOST_PID 2>/dev/null" EXIT

for i in $(seq 1 50); do
  if curl -sf "http://[::1]:$PORT/info" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

hurl --test --variable "host=http://[::1]:$PORT" "$SCRIPT_DIR"/*.hurl
