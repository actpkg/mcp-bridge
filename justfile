wasm := "target/wasm32-wasip2/release/mcp_bridge.wasm"
act := env("ACT", "act")
port := `python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_STREAM); s.bind(("", 0)); print(s.getsockname()[1]); s.close()'`
addr := "[::1]:" + port
baseurl := "http://" + addr

init:
    wit-deps

setup: init
    prek install

build:
    cargo build --target wasm32-wasip2 --release

test:
    #!/usr/bin/env bash
    {{act}} serve {{wasm}} --listen "{{addr}}" &
    trap "kill $!" EXIT
    npx wait-on {{baseurl}}/info
    hurl --test --variable "baseurl={{baseurl}}" --variable "mcp_url=https://zip1.io/mcp" e2e/*.hurl
