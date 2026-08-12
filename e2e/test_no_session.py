"""Without std:session-id, tools/list surfaces nothing upstream and
tools/call errors with std:invalid-args.
"""


async def test_tools_list_shows_only_the_virtual_session_tools(client):
    # hurl: `$.tools count == 0`. ACT-HTTP's `/tools` forwards the guest's
    # own (session-scoped) list-tools answer directly, so with no session it
    # is empty. MCP's `tools/list` has no way to carry a session id at all —
    # confirmed empirically: even a raw `tools/list` request with
    # `_meta: {"std:session-id": <a real open id>}` still returns only the
    # two synthesised virtuals, never the upstream's tools (ACT-SESSIONS
    # §6.1: the host always shows open_session/close_session for a
    # session-provider component's tools/list, session or no session). So
    # "0 tools" has no literal MCP counterpart; the faithful translation of
    # the same underlying claim — no upstream tool is reachable without a
    # session — is that the list is exactly the two virtuals, never more.
    tools = await client.list_tools()
    assert sorted(t.name for t in tools) == ["close_session", "open_session"]


async def test_call_without_session_id_is_invalid_args(client, expect_error):
    await expect_error(client, "anything", {}, "std:invalid-args", contains="std:session-id")
