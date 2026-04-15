wasm := "target/wasm32-wasip2/release/mcp_bridge.wasm"

act := env("ACT", "npx @actcore/act")
actbuild := env("ACT_BUILD", "npx @actcore/act-build")
hurl := env("HURL", "npx @orangeopensource/hurl")
oras := env("ORAS", "oras")
registry := env("OCI_REGISTRY", "ghcr.io/actpkg")
port := `npx get-port-cli`
addr := "[::1]:" + port
baseurl := "http://" + addr

init:
    wit-deps

setup: init
    prek install

build:
    cargo build --release
    {{actbuild}} pack {{wasm}}

mcpport := `npx get-port-cli`
mcpurl := "http://127.0.0.1:" + mcpport + "/mcp"

test:
    #!/usr/bin/env bash
    set -euo pipefail
    PIDS=()
    trap 'kill "${PIDS[@]}" 2>/dev/null' EXIT
    npx mcp-proxy --host 0.0.0.0 --port {{mcpport}} --stateless -- npx @trippnology/mcp-server-hello-world &
    PIDS+=($!)
    {{act}} run {{wasm}} --http --listen "{{addr}}" &
    PIDS+=($!)
    npx wait-on -t 180s "tcp:127.0.0.1:{{mcpport}}" {{baseurl}}/info
    {{hurl}} --test --variable "baseurl={{baseurl}}" --variable "mcpurl={{mcpurl}}" e2e/*.hurl

publish:
    #!/usr/bin/env bash
    set -euo pipefail
    INFO=$({{act}} info {{wasm}} --format json)
    NAME=$(echo "$INFO" | jq -r .name)
    VERSION=$(echo "$INFO" | jq -r .version)
    DESC=$(echo "$INFO" | jq -r .description)
    if {{oras}} manifest fetch "{{registry}}/$NAME:$VERSION" >/dev/null 2>&1; then
      echo "$NAME:$VERSION already published, skipping"
      exit 0
    fi
    SOURCE=$(git remote get-url origin 2>/dev/null | sed 's/\.git$//' | sed 's|git@github.com:|https://github.com/|' || echo "")
    OUTPUT=$({{oras}} push "{{registry}}/$NAME:$VERSION" \
      --artifact-type application/wasm \
      --annotation "org.opencontainers.image.version=$VERSION" \
      --annotation "org.opencontainers.image.description=$DESC" \
      --annotation "org.opencontainers.image.source=$SOURCE" \
      "{{wasm}}:application/wasm" 2>&1)
    echo "$OUTPUT"
    DIGEST=$(echo "$OUTPUT" | grep "^Digest:" | awk '{print $2}')
    {{oras}} tag "{{registry}}/$NAME:$VERSION" latest
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
      echo "image={{registry}}/$NAME" >> "$GITHUB_OUTPUT"
      echo "digest=$DIGEST" >> "$GITHUB_OUTPUT"
    fi
