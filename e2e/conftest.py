"""Shared fixtures for the MCP-driven e2e suite.

The suite drives the packed component through `act run --mcp` over stdio with
a real MCP client, so what the tests observe is what an agent observes.
"""

import asyncio
import json
import os
import random
import shlex
import socket
import subprocess
import time
import pytest
from contextlib import AsyncExitStack, asynccontextmanager, contextmanager
from pathlib import Path

from fastmcp import Client
from fastmcp.client.transports import StdioTransport

# Measured in docs/specs/2026-08-08-e2e-harness-findings.md, question 1.
from mcp.shared.exceptions import McpError

WASM = "target/wasm32-wasip2/release/mcp_bridge.wasm"
STUB_SERVER = Path(__file__).parent / "stub-mcp-server.mjs"

# ACT's audit trail writes to stderr unconditionally — it is not governed by
# RUST_LOG — so it is redirected to a file rather than left to flood pytest.
LOG_FILE = Path(".pytest-act-stderr.log")

# Deliberately loose. `act run --mcp` instantiates the component before it
# answers `initialize`, so "connect" includes that cost -- for a heavy
# component (servo embeds a browser engine) it is seconds, and on a loaded
# runner it varies. 30s tripped servo in CI while its healthy connect was
# ~8s, so the bound sits well above the worst observed cost and still well
# below the per-test timeout, keeping this the diagnostic that fires first.
CONNECT_TIMEOUT = 120


@pytest.fixture(scope="session")
def act_command() -> list[str]:
    """The ACT invocation, honouring the same override the justfile uses.

    Parsed with shlex, not treated as a single path: the justfile's own
    default for its `act` variable is `npx @actcore/act` — two words — which
    cannot be `argv[0]` for a non-shell `subprocess.run`/`StdioTransport`
    call. A bare `os.environ.get("ACT", "act")` string breaks that default;
    splitting it is what makes both forms ("act" on PATH, and the npx
    two-word default) actually spawn.
    """
    return shlex.split(os.environ.get("ACT", "act"))


@pytest.fixture(scope="session")
def wasm_path(act_command: list[str]) -> Path:
    """The packed component.

    Existence is not enough and neither is a fresh mtime: `cargo build`
    produces a wasm with no `act:component` custom section, and an unpacked
    artifact declares no capability ceiling, so every grant is refused as
    "outside ceiling" and the failures point anywhere but here. This has
    already bitten this workspace repeatedly, so the fixture checks the
    section rather than the file.
    """
    path = Path(WASM)
    if not path.exists():
        pytest.fail(f"{path} is missing — run `just build && just pack` first")
    probe = subprocess.run(
        [*act_command, "inspect", "component-manifest", str(path)],
        capture_output=True, text=True,
    )
    name = json.loads(probe.stdout or "{}").get("std", {}).get("name", "unknown")
    if name in ("", "unknown"):
        pytest.fail(f"{path} is built but not packed — run `just pack`")
    return path


def _port_open(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", port)) == 0


@pytest.fixture
def stub_server():
    """The stub-server context manager itself, handed to a test that needs to
    start one with arguments of its own (`require_token=`).

    A fixture rather than something a test imports from `conftest`: that
    import only resolves when the test directory happens to be on `sys.path`,
    which is not something to rely on.
    """
    return _stub_server


@contextmanager
def _stub_server(require_token: str | None = None):
    """Run `e2e/stub-mcp-server.mjs` and yield the URL the bridge should use.

    Factored out of `mcp_upstream` so the credential suite can start the same
    server with `--require-token`, which makes it 401 every request whose
    `Authorization` header does not match — the probe and the handshake
    included, which is exactly why the bridge cannot negotiate before it has
    fetched the credential.
    """
    port = random.randint(10000, 29999)
    argv = ["node", str(STUB_SERVER), "--port", str(port), "--mode", "modern"]
    if require_token is not None:
        argv += ["--require-token", require_token]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            if _port_open(port):
                break
            if proc.poll() is not None:
                pytest.fail(f"stub-mcp-server exited early with code {proc.returncode}")
            time.sleep(0.1)
        else:
            proc.kill()
            pytest.fail(f"stub-mcp-server did not open port {port} in time")
        yield f"http://127.0.0.1:{port}/mcp"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


@pytest.fixture
def mcp_upstream():
    """A local upstream MCP server for the bridge to proxy to.

    `e2e/stub-mcp-server.mjs` is shared with `test-dialects` (not part of
    this migration — left untouched). It is deliberately strict about the
    dialect it serves, so any request shape the bridge gets wrong fails
    loudly rather than passing against a lenient server. Run in `modern`
    mode; the dialect itself is exercised separately by `test-dialects`, so
    either mode would do here — `modern` matches the script's own default.

    Port picked the same way the justfile picks one (`shuf -i 10000-29999
    -n 1`): above common dev ports, below the Linux ephemeral range. Waits
    for the port to actually accept connections before yielding — starting
    the process and hoping for the best cost another component a red CI run
    on the exact same class of race — and is torn down unconditionally.
    """
    with _stub_server() as url:
        yield url


@pytest.fixture
def act_client(act_command: list[str], wasm_path: Path):
    """Factory for a connected MCP client: `async with act_client(*extra): ...`

    Everything a client needs is here so that a module wanting one more host
    argument — `--credentials-backend`, say — adds it rather than duplicating
    the spawn, the connect bound and the stderr redirect.

    `--allow wasi:http --allow act:credentials` are granted unconditionally.
    The bridge's only job is talking to an upstream MCP server over HTTP, and
    the credentials class is checked when the component *starts*, not per
    call — a test that never names a credential still cannot instantiate the
    guest without it, and the failure would point anywhere but here. The
    component's own ceiling scopes neither wider than it declares: a bare
    `act:credentials` table, and `wasi:http` (act.toml).
    """

    @asynccontextmanager
    async def _connect(*extra_args: str):
        transport = StdioTransport(
            command=act_command[0],
            args=[
                *act_command[1:], "run", str(wasm_path), "--mcp",
                "--allow", "wasi:http", "--allow", "act:credentials",
                *extra_args,
            ],
            keep_alive=False,
            log_file=LOG_FILE,
        )
        async with AsyncExitStack() as stack:
            # Bound the connect, not the test body. A stalled handshake
            # otherwise consumes the whole pytest timeout with no diagnostic
            # at all — which is precisely how the webdriver-bidi CI hang
            # presented for hours.
            try:
                async with asyncio.timeout(CONNECT_TIMEOUT):
                    connected = await stack.enter_async_context(Client(transport))
            except TimeoutError:
                pytest.fail(
                    f"MCP client did not connect within {CONNECT_TIMEOUT}s; "
                    f"act's stderr, if it wrote any, is dumped at session end"
                )
            yield connected

    return _connect


@pytest.fixture
async def client(act_client):
    """A connected MCP client, one `act` process per test.

    No credential store is named, so `get-secret` finds nothing here: that is
    right for every test that names no `credential_key`, and it is what makes
    the poisoned-session test able to reach an authenticated upstream with no
    token at all. The suite that needs a store builds its own client from
    `act_client`.

    Function-scoped, one client per test: a session opened in one test must
    not leak into the next. Within a single test, several calls share the
    same client/process on purpose — the session lives in the guest's
    memory for the process's lifetime, so respawning between calls would
    lose it.
    """
    async with act_client() as connected:
        yield connected


@pytest.fixture
def expect_error():
    """Assert a call fails with a specific ACT error kind (and, optionally, a
    substring of its human-readable message).

    Exposed as a fixture rather than a plain function so tests never have to
    import from `conftest` — that import only resolves when the test
    directory happens to be on `sys.path`, which is not something to rely on.

    Measured, not assumed. `call-tool` in `act:tools` returns a bare
    `tool-result` with NO `result<>` wrapper — only `list-tools` has one — so
    a guest reporting a failed tool call can only do it through
    `tool-event::error`, which arrives as a result with `is_error` set and the
    kind in `_meta`, and the message as its one text content part. **That is
    the path a tool test will take.**

    The JSON-RPC error path exists for failures that are not the guest's tool
    body: `list-tools`, the session operations, a wasmtime trap, an
    unreachable actor. It raises `mcp.shared.exceptions.McpError`, with the
    kind at `exc.error.data` and the message at `exc.error.message`. Both are
    handled here so callers need not care.

    `meta` carries `std:session-id` (and anything else) as ordinary MCP
    request `_meta` — the same channel `fastmcp.Client.call_tool`'s own
    `meta=` kwarg uses, not a key inside `arguments`. That channel keeps its
    `std:` spelling verbatim; only *response* metadata (`.meta` on the
    result, e.g. `dev.actcore/error-kind` itself) gets the `dev.actcore/`
    respelling.
    """

    async def _expect(
        client, tool: str, arguments: dict, kind: str,
        contains: str | None = None, meta: dict | None = None,
    ):
        try:
            result = await client.call_tool(tool, arguments, meta=meta, raise_on_error=False)
        except McpError as exc:
            data = getattr(getattr(exc, "error", None), "data", None) or {}
            assert data.get("dev.actcore/error-kind") == kind, (
                f"expected {kind} on the JSON-RPC error path, got {data!r}"
            )
            if contains is not None:
                message = getattr(exc.error, "message", "") or ""
                assert contains in message, f"expected {contains!r} in {message!r}"
            return

        assert result.is_error, f"expected {tool} to fail, got {result!r}"
        result_meta = result.meta or {}
        assert result_meta.get("dev.actcore/error-kind") == kind, (
            f"expected {kind} on the isError path, got {result_meta!r}"
        )
        if contains is not None:
            message = result.content[0].text if result.content else ""
            assert contains in message, f"expected {contains!r} in {message!r}"

    return _expect


def pytest_sessionfinish(session, exitstatus):
    """Print act's stderr when the run did not pass.

    `log_file` keeps the audit trail out of the test output, which is right
    for a green run and wrong for every other kind: on an ephemeral CI runner
    nothing ever reads that file. Diagnosing a CI-only hang in this fleet
    cost several rounds of probing that one line of this stream would have
    answered. A hook rather than a fixture finaliser on purpose — fixture
    teardown does not run when the session dies mid-test.
    """
    if exitstatus == 0 or not LOG_FILE.exists():
        return
    text = LOG_FILE.read_text(errors="replace").strip()
    if text:
        print(f"\n--- act stderr ({LOG_FILE}) ---\n{text}")
