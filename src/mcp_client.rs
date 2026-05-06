use schemars::JsonSchema;
use serde::Deserialize;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-session config: where to talk to the upstream MCP server.
/// Populated from `open-session.args`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "schemars", title = "mcp-bridge open-session args")]
pub struct Config {
    /// MCP server URL (e.g. http://localhost:3000/mcp)
    pub url: String,
    /// Optional Bearer token for authentication.
    pub auth_token: Option<String>,
}

#[derive(Debug)]
pub struct McpError {
    pub kind: String,
    pub message: String,
}

impl McpError {
    pub fn internal(msg: impl Into<String>) -> Self {
        McpError {
            kind: "std:internal".to_string(),
            message: msg.into(),
        }
    }

    pub fn invalid_args(msg: impl Into<String>) -> Self {
        McpError {
            kind: "std:invalid-args".to_string(),
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        McpError {
            kind: "std:not-found".to_string(),
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

// ── Per-session MCP state ──────────────────────────────────────────────────

/// Run the MCP `initialize` handshake. Returns the upstream Mcp-Session-Id
/// header value if the server issued one.
pub async fn initialize(config: &Config) -> Result<Option<String>, McpError> {
    let params = serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "act-mcp-bridge",
            "version": CLIENT_VERSION,
        },
    });
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params,
    });
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;

    let resp = http_post(config, &body_bytes, None).await?;

    let response: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| McpError::internal(format!("Invalid JSON in initialize response: {e}")))?;
    if let Some(error) = response.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("initialize failed");
        return Err(McpError::internal(msg));
    }

    // Send the initialized notification (fire-and-forget).
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    });
    let notification_bytes = serde_json::to_vec(&notification)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;
    let _ = http_post(config, &notification_bytes, resp.session_id.as_deref()).await;

    Ok(resp.session_id)
}

/// Send a JSON-RPC 2.0 request to the MCP server using the provided
/// session-id (already validated by the bridge — see open_session). On
/// HTTP 404 the upstream session expired; the caller is responsible for
/// recovery (reopening or returning std:session-not-found).
pub async fn mcp_request(
    config: &Config,
    upstream_session_id: Option<&str>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, McpError> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;

    let resp = http_post(config, &body_bytes, upstream_session_id).await?;
    let response: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| McpError::internal(format!("Invalid JSON response: {e}")))?;

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        return Err(match code {
            -32600 | -32602 => McpError::invalid_args(message),
            -32601 => McpError::not_found(message),
            _ => McpError::internal(message),
        });
    }

    response
        .get("result")
        .cloned()
        .ok_or_else(|| McpError::internal("JSON-RPC response missing 'result' field"))
}

/// Best-effort upstream session close (DELETE /mcp with the session
/// header). Cleanup errors are swallowed by design — close-session is
/// advisory in WIT.
pub async fn close_upstream(config: &Config, upstream_session_id: &str) {
    let mut builder = wasi_fetch::Client::new()
        .delete(&config.url)
        .header("accept", "application/json")
        .header(SESSION_HEADER, upstream_session_id)
        .timeout(std::time::Duration::from_secs(5));
    if let Some(ref token) = config.auth_token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let _ = builder.send().await;
}

// ── HTTP transport ──────────────────────────────────────────────────────────

const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const SESSION_HEADER: &str = "mcp-session-id";

struct HttpResponse {
    body: Vec<u8>,
    session_id: Option<String>,
}

/// Parse SSE events: find the first event with a non-empty `data:` field.
fn parse_sse_data(text: &str) -> Option<String> {
    let normalized;
    let text = if text.contains('\r') {
        normalized = text.replace("\r\n", "\n");
        normalized.as_str()
    } else {
        text
    };
    for event_block in text.split("\n\n") {
        let mut data = String::new();
        for line in event_block.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                let value = value.trim_start();
                if !value.is_empty() {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
            }
        }
        if !data.is_empty() {
            return Some(data);
        }
    }
    None
}

/// Read an SSE response chunk-by-chunk until the first complete event.
async fn read_sse_event(mut body: wasi_fetch::Body) -> Result<Vec<u8>, McpError> {
    let mut buf = Vec::new();
    while let Some(chunk) = body.chunk().await {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(McpError::internal("MCP response too large"));
        }
        if let Ok(text) = std::str::from_utf8(&buf)
            && let Some(data) = parse_sse_data(text)
        {
            return Ok(data.into_bytes());
        }
    }
    Err(McpError::internal("SSE stream ended without a data event"))
}

/// Low-level HTTP POST using wasi-fetch (Streamable HTTP transport).
async fn http_post(
    config: &Config,
    body_bytes: &[u8],
    session_id: Option<&str>,
) -> Result<HttpResponse, McpError> {
    let mut builder = wasi_fetch::Client::new()
        .post(&config.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body_bytes.to_vec())
        .timeout(std::time::Duration::from_secs(30));

    if let Some(ref token) = config.auth_token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(sid) = session_id {
        builder = builder.header(SESSION_HEADER, sid);
    }

    let response = builder
        .send()
        .await
        .map_err(|e| McpError::internal(format!("HTTP error: {e}")))?;

    let status = response.status().as_u16();
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    let resp_session_id = response
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if is_sse {
        if !(200..300).contains(&status) {
            return Err(McpError::internal(format!("HTTP {status} from MCP server")));
        }
        Ok(HttpResponse {
            body: read_sse_event(response.into_body()).await?,
            session_id: resp_session_id,
        })
    } else {
        let body = response.into_body().bytes().await;
        if !(200..300).contains(&status) {
            let detail = String::from_utf8_lossy(&body);
            return Err(McpError::internal(format!(
                "HTTP {status} from MCP server: {detail}"
            )));
        }
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(McpError::internal("MCP response too large"));
        }
        Ok(HttpResponse {
            body: body.to_vec(),
            session_id: resp_session_id,
        })
    }
}
