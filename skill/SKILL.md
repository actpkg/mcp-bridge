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
| `url` | string | yes | MCP server endpoint (e.g. `http://localhost:3000/mcp`). Must not carry userinfo |
| `credential_key` | string | no | Names the credential in this component's profile that authenticates the upstream. Omit for a server that needs no token |
| `protocol_version` | string | no | Pin the dialect: `2025-11-25` or `2026-07-28`. Omit to auto-detect |

**There is no `auth_token` argument, and there must never be one.**
Everything sent to `open-session` is agent-visible plaintext that lands
in the transcript and in the host's session record. The token lives in
the host credential store and is named, not passed — see *Credentials*
below.

`open-session` validates its arguments and returns an id. It does **not**
contact the upstream: the dialect probe needs the bearer token, and the
token cannot be fetched until the session is live, which the host only
declares after `open-session` returns (ACT-AUTH §1.1.4). The handshake
therefore runs on the **first tool call**, and that is where a bad URL, a
401 or a protocol mismatch surfaces. `close-session` sends a best-effort
`DELETE` to release the upstream session, if one was ever established.

Without `std:session-id`, `list-tools` returns an empty list and
`call-tool` errors with `std:invalid-args`. Calls referencing a
closed session-id return `std:session-not-found`.

## Credentials

The upstream bearer token comes from the host credential store
(`act:credentials@0.1.0`), so it never passes through the agent's
context. The agent names a key; the operator provisions the value out of
band.

Two field names, and the one that is stored decides how the token is
read — the type is a property of the **field**, and nothing here infers a
credential's meaning from its shape (`ACT-CONSTANTS.md` §8.1–8.2):

| field | type | when |
| --- | --- | --- |
| `mcp:token` | `std:string` | a bearer token pasted by hand |
| `mcp:oauth` | `std:oauth2` | an access token from an OAuth flow, with its expiry |

```bash
# a plain bearer token
act secret set <component-ref> --key upstream --field mcp:token --fields-stdin
# {"mcp:token":"<token>"}

# an OAuth credential (map members per ACT-CONSTANTS §8.3)
act secret set <component-ref> --key upstream --field mcp:oauth=std:oauth2 --fields-stdin
# {"mcp:oauth":{"std:access-token":"<token>","std:expires-at":1760000000,"std:scopes":["repo"]}}
```

Then open the session with `{"url": "...", "credential_key": "upstream"}`.

Storing both fields under one key is refused: they are two credentials,
and which one is live is not a detail to resolve by precedence.

An `mcp:oauth` token past its `std:expires-at` is refused **before** the
request goes out, with `std:credential-required`. ACT does not refresh a
stored token — silent refresh is out of scope for the host (ACT-AUTH
§1.1) — so re-acquire it with `act login <ref> --key <key> --field
mcp:oauth --force` and open a new session.

A credential refused by the host or by the upstream **poisons the
session**: the same error is returned to every later call rather than
re-fetching. The key is fixed at open and nothing refreshes it, so a
retry could only re-run `get-secret`, which can put a consent prompt in
front of a human on every single call. Fix the credential, then open a
new session.

### Granting it

```bash
act run <ref> --mcp \
  --allow act:credentials \
  --grant '{"wasi:http":{"mode":"allowlist","allow":[{"host":"mcp.example.com"}]}}'
```

`--allow act:credentials` is required — the class is declared as a bare
table and an undeclared or ungranted class is denied outright.

Prefer the `--grant` allowlist above over a bare `--allow wasi:http`.
This component's declared HTTP ceiling is `host = "*"`, because a bridge
has to be able to reach whatever server it is pointed at; granting it
open means the artifact's own declaration is the only bound on where a
credential it holds could be sent. `act` warns about exactly this
combination.

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

HTTP status, on the other hand:

| upstream status | ACT error kind |
| --- | --- |
| `401` / `403` | `std:capability-denied` (and the session is poisoned) |
| other non-2xx | `std:internal` |

Credential failures on this component's own side are
`std:credential-required` (`ACT-CONSTANTS.md` §9): the key names nothing,
the stored fields are not readable as a token, or an OAuth token has
expired. Every one of those messages names the command that fixes it.

## Example

```text
open_session({"url": "https://mcp.example.com/mcp", "credential_key": "upstream"})
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
- Only `Authorization: Bearer` is supported upstream. A server wanting
  Basic, mTLS or a vendor header cannot be bridged.
- No token refresh. An expired `mcp:oauth` credential is re-acquired with
  `act login`, not renewed in place.
- The upstream is not reached until the first tool call, so `open_session`
  succeeding says nothing about whether the server is up.
