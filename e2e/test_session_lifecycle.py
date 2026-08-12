"""One client, one session, walked end to end — mirrors the single hurl file
this replaces, where later blocks depend on state (the session id) captured
from an earlier one. Split across separate test functions it would either
respawn the process (losing the session) or duplicate the open/close dance
per assertion; kept as one test instead, the same way the hurl file was one
file.
"""

import json
import re


async def test_session_lifecycle(client, expect_error, mcp_upstream):
    # ── open-session args schema (driven by Config / schemars) ──────────
    tools = await client.list_tools()
    open_tool = next(t for t in tools if t.name == "open_session")
    assert open_tool.inputSchema["type"] == "object"
    assert "url" in open_tool.inputSchema["properties"]

    # ── Open a session against the upstream MCP server ────────────────────
    # The bridge runs the initialize/discover handshake and stashes the
    # upstream Mcp-Session-Id (if any) before returning its own id.
    result = await client.call_tool("open_session", {"url": mcp_upstream})
    session_id = json.loads(result.content[0].text)["id"]
    # hurl: `matches "mcp_\\d+"`. hurl's `matches` is an unanchored substring
    # search (measured empirically during the `time` migration, not assumed),
    # so re.search is the faithful translation, not re.fullmatch.
    assert re.search(r"mcp_\d+", session_id)

    # ── tools/list "scoped to this session" ────────────────────────────────
    # hurl asserted `count >= 1` and `contains "echo"` here: ACT-HTTP's
    # `/tools` forwards the guest's session-scoped answer directly. MCP's
    # `tools/list` cannot carry a session id at all — measured directly
    # against the raw protocol: a `tools/list` request with
    # `_meta: {"std:session-id": session_id}` still returns only the two
    # virtuals. The host always shows open_session/close_session for a
    # session-provider component's tools/list, session open or not (per
    # ACT-SESSIONS §6.1 — the virtual tools ARE the list, not an addition to
    # it). So the two original assertions have no literal MCP counterpart;
    # asserting them as originally phrased would assert something false. The
    # faithful translation of the *underlying* claim — that the upstream's
    # tools are reachable through this session — is the actual `echo` call
    # two steps down, which invokes it successfully by name despite it never
    # appearing here.
    tools = await client.list_tools()
    assert sorted(t.name for t in tools) == ["close_session", "open_session"]
    assert "echo" not in [t.name for t in tools]

    # ── tools/call -> echo ──────────────────────────────────────────────────
    result = await client.call_tool("echo", {"message": "World"}, meta={"std:session-id": session_id})
    assert "World" in result.content[0].text

    # ── Unknown tool surfaces as an error from the upstream ──────────────────
    # hurl only asserted `$.error exists` (any error). Measured the actual
    # kind directly: the stub's own comment documents SEP-2164 folding "no
    # such thing" onto JSON-RPC -32602, and the bridge must still classify it
    # as std:not-found rather than std:invalid-args — confirmed against the
    # running component. Checking the specific kind is a strictly stronger,
    # still-true version of the original assertion, not an invented scenario.
    await expect_error(
        client, "nonexistent_tool_xyz", {}, "std:not-found", meta={"std:session-id": session_id},
    )

    # ── Close the session — fire-and-forget upstream DELETE ─────────────────
    await client.call_tool("close_session", {"session_id": session_id})

    # ── Calls referencing the closed id surface std:session-not-found ────────
    await expect_error(
        client, "echo", {"message": "after close"}, "std:session-not-found",
        meta={"std:session-id": session_id},
    )
