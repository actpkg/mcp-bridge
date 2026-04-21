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
    {{act}} run {{wasm}} --http --listen "{{addr}}" --http-policy open &
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
    # If this version is already published, require that its content matches
    # our local build exactly. A silent skip when content drifted would mean
    # the registry lies to downstream consumers (e.g. actpkg.dev).
    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT
    if {{act}} pull "{{registry}}/$NAME:$VERSION" -o "$TMPDIR/remote.wasm" >/dev/null 2>&1; then
      LOCAL_HASH=$(sha256sum {{wasm}} | awk '{print $1}')
      REMOTE_HASH=$(sha256sum "$TMPDIR/remote.wasm" | awk '{print $1}')
      if [ "$LOCAL_HASH" = "$REMOTE_HASH" ]; then
        echo "$NAME:$VERSION already published (identical content), skipping"
        exit 0
      fi
      echo "ERROR: $NAME:$VERSION is already published, but its content differs from the local build." >&2
      echo "       Bump the patch version in Cargo.toml — a metadata-only change (capabilities," >&2
      echo "       description, skill/) still requires a version bump to reach the registry." >&2
      echo "       Local:  $LOCAL_HASH" >&2
      echo "       Remote: $REMOTE_HASH" >&2
      exit 1
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
