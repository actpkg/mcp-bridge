# open-session args schema (driven by Config / schemars).
POST {{baseurl}}/sessions/open-args-schema
{}
HTTP 200
[Asserts]
jsonpath "$.type" == "object"
jsonpath "$.properties.url" exists

# Open a session against the upstream MCP server. The bridge runs the
# initialize handshake and stashes the upstream Mcp-Session-Id (if any)
# before returning its own id.
POST {{baseurl}}/sessions
{"arguments": {"url": "{{mcpurl}}"}}
HTTP 201
[Asserts]
jsonpath "$.id" matches "mcp_\\d+"
[Captures]
session_id: jsonpath "$.id"

# tools/list, scoped to this session.
POST {{baseurl}}/tools
{"metadata": {"std:session-id": "{{session_id}}"}}
HTTP 200
[Asserts]
jsonpath "$.tools" count >= 1
jsonpath "$.tools[*].name" contains "echo"

# tools/call → echo.
POST {{baseurl}}/tools/echo
{"arguments": {"message": "World"}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP 200
[Asserts]
jsonpath "$.content[0].data" contains "World"

# Unknown tool surfaces as an error from the upstream.
POST {{baseurl}}/tools/nonexistent_tool_xyz
{"arguments": {}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP *
[Asserts]
jsonpath "$.error" exists

# Close the session — fire-and-forget upstream DELETE.
DELETE {{baseurl}}/sessions/{{session_id}}
HTTP 204

# Calls referencing the closed id surface std:session-not-found (404).
POST {{baseurl}}/tools/echo
{"arguments": {"message": "after close"}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP 404
[Asserts]
jsonpath "$.error.kind" == "std:session-not-found"
