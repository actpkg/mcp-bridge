# mcp-bridge

Proxy a **remote** MCP server's tools as native ACT tools.

The bridge speaks Streamable HTTP to an upstream MCP server and re-exposes
whatever that server publishes, so an agent reaches it through the same
`act:tools` surface as any other component — with the host's capability
ceiling, audit trail and credential store in front of it.

It bridges *remote* servers only. It does not run, contain or sandbox a
third-party MCP server: the server it talks to keeps running wherever it
already runs, with whatever access it already has.

## Credentials

The upstream's bearer token is **not** a session argument. It lives in the
host credential store, is *named* by `credential_key`, and is fetched with
`act:credentials@0.1.0` on the first tool call — so it never passes through
the agent's context, the transcript, or the host's session record.

Two field names; the one that is stored decides how the token is read. The
type is a property of the **field** (`ACT-CONSTANTS.md` §8.1) and nothing here
infers a credential's meaning from its shape (§8.2):

| field | type | when |
| --- | --- | --- |
| `mcp:token` | `std:string` | a bearer token pasted by hand |
| `mcp:oauth` | `std:oauth2` | an access token from an OAuth flow, with its expiry |

```bash
# a plain bearer token
act secret set actpkg.dev/library/mcp-bridge --key upstream \
  --field mcp:token --fields-stdin
# {"mcp:token":"<token>"}

# an OAuth credential — map members per ACT-CONSTANTS §8.3
act secret set actpkg.dev/library/mcp-bridge --key upstream \
  --field mcp:oauth=std:oauth2 --fields-stdin
# {"mcp:oauth":{"std:access-token":"<token>","std:expires-at":1760000000}}
```

`credential_key` is **optional**: a bridge pointed at an unauthenticated MCP
server names none, and nothing is fetched or prompted for.

An `mcp:oauth` token past its `std:expires-at` is refused before the request
goes out. ACT does not refresh a stored token — silent refresh is out of scope
for the host (ACT-AUTH §1.1) — so re-acquire it with
`act login <ref> --key <key> --field mcp:oauth --force` and open a new session.

## Usage

```bash
act run actpkg.dev/library/mcp-bridge --mcp \
  --allow act:credentials \
  --grant '{"wasi:http":{"mode":"allowlist","allow":[{"host":"mcp.example.com"}]}}'
```

then, as the agent:

```text
open_session({"url": "https://mcp.example.com/mcp", "credential_key": "upstream"})
→ {"id": "mcp_0"}
call_tool("echo", {"message": "hi"}, _meta = {"std:session-id": "mcp_0"})
```

Or as a one-shot, which opens a session of one:

```bash
act call actpkg.dev/library/mcp-bridge echo --args '{"message":"hi"}' \
  --session-args '{"url":"https://mcp.example.com/mcp","credential_key":"upstream"}' \
  --allow act:credentials \
  --grant '{"wasi:http":{"mode":"allowlist","allow":[{"host":"mcp.example.com"}]}}'
```

**Prefer that `--grant` allowlist over a bare `--allow wasi:http`.** This
component's declared HTTP ceiling is `host = "*"` — a bridge has to reach
whatever server it is pointed at — so granting it open leaves the artifact's
own declaration as the only bound on where a credential it holds could be
sent. `act` warns about exactly this combination.

`--allow act:credentials` is required whenever `credential_key` is used: the
class is declared as a bare table, and an undeclared or ungranted class is
denied outright.

`open_session` validates its arguments and returns an id **without contacting
the upstream**. The dialect probe needs the bearer token, and the token cannot
be fetched until the session is live — which the host declares only after
`open-session` returns (ACT-AUTH §1.1.4). The handshake therefore runs on the
first tool call, and that is where a bad URL, a 401 or a protocol mismatch
surfaces.

See `skill/SKILL.md` for the tool catalogue, the annotation and content-type
mappings, the error mapping and the limitations.

## Development

```bash
just init   # first time: fetch WIT deps into wit/deps/
just build  # cargo build --release + act-build pack
just test   # unit tests, the dual-dialect drive, then the MCP-driven e2e suite
```

`ACT`/`ACT_BUILD` default to `npx @actcore/act`/`npx @actcore/act-build`;
override them to point at a local binary, e.g. `export ACT=act
ACT_BUILD=act-build`.

`just test` needs an `act` that implements the `act:credentials` host import —
this component imports `act:credentials/store@0.1.0`, and a host without it
cannot instantiate the component at all. The credential suite additionally
drives `act secret set --credentials-backend file:<tmp>`; it skips itself (and
only itself) when `act secret` is missing.
