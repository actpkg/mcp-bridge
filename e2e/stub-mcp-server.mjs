// Minimal MCP server used by the e2e suite to exercise both protocol
// dialects the bridge speaks. No dependencies — plain node:http.
//
//   node stub-mcp-server.mjs --port <n> --mode legacy|modern
//
// The stub is deliberately strict: whenever the bridge sends a request that
// does not match the dialect it negotiated, the stub answers with a JSON-RPC
// error instead of a result, so the hurl assertions fail loudly rather than
// passing on a lenient server.
//
//   legacy (2025-11-25) — `server/discover` is answered with -32601, so the
//     bridge must fall back to the `initialize` handshake. The stub issues an
//     `Mcp-Session-Id` and rejects any later request that omits it.
//   modern (2026-07-28) — `server/discover` advertises the revision. The stub
//     then requires the SEP-2243 `Mcp-Method` / `Mcp-Name` headers and the
//     SEP-2575 `_meta` keys on every request, and rejects a request that
//     carries an `Mcp-Session-Id` at all.

import { createServer } from "node:http";

const args = process.argv.slice(2);
const argOf = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};
const PORT = Number(argOf("--port", "0"));
const MODE = argOf("--mode", "modern");
const SESSION_ID = "stub-session-1";
const MODERN_VERSION = "2026-07-28";
const LEGACY_VERSION = "2025-11-25";

const META_PROTOCOL_VERSION = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_INFO = "io.modelcontextprotocol/clientInfo";
const META_CLIENT_CAPABILITIES = "io.modelcontextprotocol/clientCapabilities";

const TOOLS = [
  {
    name: "echo",
    description: "Echo a message back",
    inputSchema: {
      type: "object",
      properties: { message: { type: "string" } },
      required: ["message"],
    },
  },
  {
    // No `description`; the bridge falls back to `title`.
    name: "titled",
    title: "Titled Tool",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "structured",
    description: "Return structuredContent plus its text mirror",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "rich",
    description: "Return an audio block and a resource link",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "needs_input",
    description: "Answer with an MRTR input-required result (SEP-2322)",
    inputSchema: { type: "object", properties: {} },
  },
];

function callTool(name, args) {
  switch (name) {
    case "echo":
      return { content: [{ type: "text", text: `Hello ${args?.message ?? ""}` }] };
    case "titled":
      return { content: [{ type: "text", text: "titled" }] };
    case "structured":
      return {
        content: [{ type: "text", text: '{"temp":22.5}' }],
        structuredContent: { temp: 22.5 },
      };
    case "rich":
      return {
        content: [
          // "RIFF" in base64.
          { type: "audio", data: "UklGRg==", mimeType: "audio/wav" },
          {
            type: "resource_link",
            uri: "file:///report.pdf",
            name: "report.pdf",
            description: "Quarterly report",
            mimeType: "application/pdf",
            size: 4096,
          },
        ],
      };
    case "needs_input":
      return {
        resultType: "input_required",
        inputRequests: {
          confirm: {
            method: "elicitation/create",
            params: { message: "Are you sure?", requestedSchema: { type: "object" } },
          },
        },
        requestState: "opaque-state",
      };
    default:
      return null;
  }
}

const err = (id, code, message) => ({ jsonrpc: "2.0", id: id ?? null, error: { code, message } });
const ok = (id, result) => ({ jsonrpc: "2.0", id: id ?? null, result });

/// Reject a request that does not match the dialect the stub is serving.
/// Returns an error message, or null when the request is well-formed.
function violation(method, params, headers) {
  const headerMethod = headers["mcp-method"];
  const headerName = headers["mcp-name"];
  const sessionId = headers["mcp-session-id"];

  if (MODE === "modern") {
    if (sessionId !== undefined) {
      return `2026-07-28 requests must not carry Mcp-Session-Id (got ${sessionId})`;
    }
    if (headerMethod !== method) {
      return `Mcp-Method header ${headerMethod} does not match body method ${method}`;
    }
    if (method === "tools/call" && headerName !== params?.name) {
      return `Mcp-Name header ${headerName} does not match params.name ${params?.name}`;
    }
    if (method !== "tools/call" && headerName !== undefined) {
      return `Mcp-Name header sent for ${method}, which carries no name`;
    }
    const meta = params?._meta;
    for (const key of [META_PROTOCOL_VERSION, META_CLIENT_INFO, META_CLIENT_CAPABILITIES]) {
      if (meta?.[key] === undefined) return `missing required request _meta key ${key}`;
    }
    if (meta[META_PROTOCOL_VERSION] !== MODERN_VERSION) {
      return `_meta protocolVersion is ${meta[META_PROTOCOL_VERSION]}, expected ${MODERN_VERSION}`;
    }
    return null;
  }

  // legacy
  if (headerMethod !== undefined || headerName !== undefined) {
    return "SEP-2243 headers must not be sent to a 2025-11-25 server";
  }
  if (params?._meta?.[META_PROTOCOL_VERSION] !== undefined) {
    return "SEP-2575 client context must not be sent to a 2025-11-25 server";
  }
  if (method !== "initialize" && sessionId !== SESSION_ID) {
    return `missing or wrong Mcp-Session-Id (got ${sessionId})`;
  }
  return null;
}

function handle(body, headers, res) {
  const { id, method, params } = body;

  // `server/discover` is the dialect probe. A legacy server has never heard
  // of it and answers method-not-found.
  if (method === "server/discover") {
    if (MODE === "legacy") return err(id, -32601, "Method not found: server/discover");
    const problem = violation(method, params, headers);
    if (problem) return err(id, -32020, problem);
    return ok(id, {
      resultType: "complete",
      supportedVersions: [MODERN_VERSION],
      capabilities: { tools: {} },
      serverInfo: { name: "stub-mcp-server", version: "0.0.0" },
      ttlMs: 0,
      cacheScope: "private",
    });
  }

  if (method === "initialize") {
    if (MODE === "modern") return err(id, -32601, "2026-07-28 removed the initialize handshake");
    const problem = violation(method, params, headers);
    if (problem) return err(id, -32020, problem);
    res.setHeader("mcp-session-id", SESSION_ID);
    return ok(id, {
      protocolVersion: LEGACY_VERSION,
      capabilities: { tools: {} },
      serverInfo: { name: "stub-mcp-server", version: "0.0.0" },
    });
  }

  const problem = violation(method, params, headers);
  if (problem) return err(id, -32020, problem);

  if (method === "notifications/initialized") return null; // notification: no response
  if (method === "tools/list") return ok(id, { tools: TOOLS });
  if (method === "tools/call") {
    const result = callTool(params?.name, params?.arguments);
    // SEP-2164 folded "no such thing" onto -32602; the bridge must still
    // classify it as std:not-found rather than std:invalid-args.
    if (result === null) return err(id, -32602, `Unknown tool: ${params?.name}`);
    return ok(id, result);
  }
  return err(id, -32601, `Method not found: ${method}`);
}

createServer((req, res) => {
  if (req.method === "DELETE") {
    res.writeHead(204).end();
    return;
  }
  let raw = "";
  req.on("data", (chunk) => (raw += chunk));
  req.on("end", () => {
    let body;
    try {
      body = JSON.parse(raw);
    } catch {
      res.writeHead(400).end("bad json");
      return;
    }
    const response = handle(body, req.headers, res);
    if (response === null) {
      res.writeHead(202).end();
      return;
    }
    const payload = JSON.stringify(response);
    res.writeHead(200, { "content-type": "application/json" }).end(payload);
  });
}).listen(PORT, "127.0.0.1", () => {
  process.stdout.write(`stub-mcp-server mode=${MODE} port=${PORT}\n`);
});
