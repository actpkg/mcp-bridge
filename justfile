wasm := "target/wasm32-wasip2/release/mcp_bridge.wasm"

act := env("ACT", "npx @actcore/act")
actbuild := env("ACT_BUILD", "npx @actcore/act-build")
hurl := env("HURL", "hurl")
registry := env("OCI_REGISTRY", "actpkg.dev/library")
port := `shuf -i 10000-29999 -n 1`
addr := "[::1]:" + port
baseurl := "http://" + addr

# Fetch WIT deps from the registry (ghcr.io/actcore) into wit/deps/.
# wkg-registry.toml maps the act namespace -> actcore.dev (well-known -> ghcr.io/actcore).
init:
    WKG_CONFIG_FILE=wkg-registry.toml wkg wit fetch --type wit

setup: init
    prek install

build:
    cargo build --release
    {{actbuild}} pack {{wasm}}

mcpport := `shuf -i 10000-29999 -n 1`
mcpurl := "http://127.0.0.1:" + mcpport + "/mcp"

test:
    #!/usr/bin/env bash
    set -euo pipefail
    PIDS=()
    trap 'kill "${PIDS[@]}" 2>/dev/null' EXIT
    npx mcp-proxy --host 0.0.0.0 --port {{mcpport}} --stateless -- npx @trippnology/mcp-server-hello-world &
    PIDS+=($!)
    {{act}} run {{wasm}} --http --listen "{{addr}}" --allow wasi:http &
    PIDS+=($!)
    # wait for the upstream MCP proxy (tcp) + the bridge's HTTP transport (/info)
    for _ in $(seq 1 120); do (echo > /dev/tcp/127.0.0.1/{{mcpport}}) >/dev/null 2>&1 && break; sleep 1; done
    curl --retry 60 --retry-connrefused --retry-delay 1 -fsS -o /dev/null {{baseurl}}/info
    {{hurl}} --test --variable "baseurl={{baseurl}}" --variable "mcpurl={{mcpurl}}" e2e/*.hurl

publish:
    #!/usr/bin/env bash
    set -euo pipefail
    INFO=$({{act}} inspect component-manifest {{wasm}})
    NAME=$(echo "$INFO" | jq -r .std.name)
    VERSION=$(echo "$INFO" | jq -r .std.version)
    OUTPUT=$({{actbuild}} push {{wasm}} "{{registry}}/$NAME:$VERSION" \
      --skip-if-exists \
      --also-tag latest 2>&1) || { echo "$OUTPUT" >&2; exit 1; }
    echo "$OUTPUT"
    DIGEST=$(echo "$OUTPUT" | grep "^Digest:" | awk '{print $2}' || true)
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
      echo "image={{registry}}/$NAME" >> "$GITHUB_OUTPUT"
      echo "digest=$DIGEST" >> "$GITHUB_OUTPUT"
    fi
