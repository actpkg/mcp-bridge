---
name: mcp-bridge
description: Bridge to remote MCP servers — proxy any MCP tools/call endpoint as native ACT tools
metadata:
  act: {}
---

# MCP Bridge Component

Connect to a remote MCP server (Streamable HTTP transport) and expose all its tools as native ACT tools. The bridge handles protocol negotiation, type mapping, and error translation automatically.

## Configuration

Requires metadata on every request:

| Key | Type | Required | Description |
|-----|------|----------|-------------|
| `url` | string | yes | MCP server endpoint (e.g. `http://localhost:3000/mcp`) |
| `auth_token` | string | no | Bearer token for authentication |

The bridge performs MCP `initialize` + `notifications/initialized` handshake on each `list-tools` and `call-tool` invocation.

## How It Works

1. **`list-tools`** — sends `tools/list` to the MCP server, maps each MCP tool definition to an ACT `ToolDefinition` (input schema, annotations)
2. **`call-tool`** — decodes dCBOR arguments to JSON, sends `tools/call`, maps the MCP response back to ACT `StreamEvent`s

### MCP Annotation Mapping

MCP tool annotations are preserved as ACT metadata:

| MCP Annotation | ACT Metadata Key |
|----------------|-------------------|
| `readOnlyHint: true` | `std:read-only` |
| `idempotentHint: true` | `std:idempotent` |
| `destructiveHint: true` | `std:destructive` |

### Content Type Mapping

| MCP Content Type | ACT Content |
|------------------|-------------|
| `TextContent` | `text/plain` data |
| `ImageContent` | Binary data with original MIME type |
| `ResourceContent` (text) | Text data with resource MIME type |
| `ResourceContent` (blob) | Binary data with resource MIME type |

### Error Mapping

MCP `isError: true` results are translated to ACT `StreamEvent::Error`. JSON-RPC error codes map to ACT error kinds:

| JSON-RPC Code | ACT Error Kind |
|---------------|----------------|
| `-32601` (method not found) | `std:not-found` |
| `-32602` (invalid params) | `std:invalid-args` |
| other | `std:internal` |

## Examples

**List tools from a local MCP server:**
```
# metadata: url = "http://localhost:3000/mcp"
list-tools → [tool definitions from remote server]
```

**Call a proxied tool with authentication:**
```
# metadata: url = "https://mcp.example.com/mcp", auth_token = "sk-..."
call-tool(name: "search", arguments: {"query": "hello"})
→ streamed results from remote MCP server
```

## Limitations

- Streamable HTTP transport only (no stdio, no legacy SSE)
- Stateless — initializes a new MCP session per request (no session reuse)
- Response size capped at 10 MB
- 30-second HTTP timeout per request
