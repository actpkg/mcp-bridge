---
name: mcp-bridge
description: Bridge to remote MCP servers — proxy any MCP tools/call endpoint as native ACT tools
metadata:
  act: {}
---

# MCP Bridge Component

Connect to a remote MCP server (Streamable HTTP transport) and expose
all its tools as native ACT tools. The bridge owns the MCP `initialize`
handshake and the `Mcp-Session-Id` header lifecycle so callers don't
have to.

## How sessions work here

This component requires a session. Open one against the upstream MCP
server you want to proxy, then thread the returned id into every tool
call as `std:session-id` metadata.

Open-session args:

| field | type | required | description |
| --- | --- | --- | --- |
| `url` | string | yes | MCP server endpoint (e.g. `http://localhost:3000/mcp`) |
| `auth_token` | string | no | Bearer token for upstream authentication |

`open-session` runs the MCP `initialize` + `notifications/initialized`
handshake against the upstream and stashes the resulting
`Mcp-Session-Id` (if the server issues one). `close-session` sends
a best-effort `DELETE` to release the upstream session.

Without `std:session-id`, `list-tools` returns an empty list and
`call-tool` errors with `std:invalid-args`. Calls referencing a
closed session-id return `std:session-not-found` (HTTP 404).

## MCP annotation mapping

MCP tool annotations are preserved as ACT metadata:

| MCP annotation | ACT metadata key |
| --- | --- |
| `readOnlyHint: true` | `std:read-only` |
| `idempotentHint: true` | `std:idempotent` |
| `destructiveHint: true` | `std:destructive` |

## Content type mapping

| MCP content type | ACT content |
| --- | --- |
| `TextContent` | `text/plain` |
| `ImageContent` | binary data with original MIME |
| `ResourceContent` (text) | text data with resource MIME |
| `ResourceContent` (blob) | binary data with resource MIME |

## Error mapping

MCP `isError: true` results become `tool-event::error`. JSON-RPC codes:

| JSON-RPC code | ACT error kind |
| --- | --- |
| `-32601` (method not found) | `std:not-found` |
| `-32600` / `-32602` | `std:invalid-args` |
| other | `std:internal` |

## Example

```text
open_session({"url": "https://mcp.example.com/mcp", "auth_token": "sk-..."})
→ {"id": "mcp_0", "metadata": {}}

list_tools(_meta = {std:session-id: "mcp_0"})
→ [echo, search, ...]

call_tool("echo", {"message": "hi"}, _meta = {std:session-id: "mcp_0"})
→ "hi"

close_session("mcp_0")
```

## Limitations

- Streamable HTTP transport only (no stdio, no legacy SSE).
- Response size capped at 10 MB.
- 30-second HTTP timeout per request.
