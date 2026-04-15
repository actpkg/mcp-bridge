use schemars::JsonSchema;
use serde::Deserialize;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize, JsonSchema)]
pub struct Config {
    /// MCP server URL (e.g. http://localhost:3000/mcp)
    pub url: String,
    /// Optional Bearer token for authentication
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

/// Extract Config from metadata key-value pairs.
/// Each value is CBOR-encoded.
pub fn parse_config_from_metadata(metadata: &[(String, Vec<u8>)]) -> Result<Config, McpError> {
    let url = metadata
        .iter()
        .find(|(k, _)| k == "url")
        .map(|(_, v)| act_types::cbor::from_cbor::<String>(v))
        .transpose()
        .map_err(|e| McpError::invalid_args(format!("Invalid url in metadata: {e}")))?
        .ok_or_else(|| McpError::invalid_args("Missing 'url' in metadata"))?;

    let auth_token = metadata
        .iter()
        .find(|(k, _)| k == "auth_token")
        .map(|(_, v)| act_types::cbor::from_cbor::<String>(v))
        .transpose()
        .map_err(|e| McpError::invalid_args(format!("Invalid auth_token in metadata: {e}")))?;

    Ok(Config { url, auth_token })
}

/// Send a JSON-RPC 2.0 request to the MCP server and return the result.
pub async fn mcp_request(
    config: &Config,
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

    let response_bytes = http_post(config, &body_bytes).await?;

    let response: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| McpError::internal(format!("Invalid JSON response: {e}")))?;

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        return Err(match code {
            -32601 => McpError::not_found(message),
            -32602 => McpError::invalid_args(message),
            _ => McpError::internal(message),
        });
    }

    response
        .get("result")
        .cloned()
        .ok_or_else(|| McpError::internal("JSON-RPC response missing 'result' field"))
}

/// Send initialize handshake to the MCP server.
pub async fn initialize(config: &Config) -> Result<(), McpError> {
    let params = serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "act-mcp-bridge",
            "version": CLIENT_VERSION,
        },
    });

    let _result = mcp_request(config, "initialize", params).await?;

    // Send initialized notification (fire-and-forget, no id field)
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    });
    let notification_bytes = serde_json::to_vec(&notification)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;

    // Fire and forget — ignore errors on the notification
    let _ = http_post(config, &notification_bytes).await;

    Ok(())
}

const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Parse SSE events: find the first event with a non-empty `data:` field.
///
/// SSE streams may contain multiple events separated by blank lines.
/// We look for the first one carrying actual JSON-RPC data.
fn parse_sse_data(text: &str) -> Option<String> {
    // Normalize \r\n to \n before splitting into event blocks
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
///
/// SSE connections use `keep-alive`, so we read incrementally and return
/// as soon as we have a complete event (data followed by an empty line).
async fn read_sse_event(mut body: wasi_fetch::Body) -> Result<Vec<u8>, McpError> {
    let mut buf = Vec::new();
    while let Some(chunk) = body.chunk().await {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(McpError::internal("MCP response too large"));
        }
        // Try to find a complete SSE event with non-empty data
        if let Ok(text) = std::str::from_utf8(&buf) {
            if let Some(data) = parse_sse_data(text) {
                return Ok(data.into_bytes());
            }
        }
    }
    Err(McpError::internal("SSE stream ended without a data event"))
}

/// Low-level HTTP POST using wasi-fetch (Streamable HTTP transport).
async fn http_post(config: &Config, body_bytes: &[u8]) -> Result<Vec<u8>, McpError> {
    let mut builder = wasi_fetch::Client::new()
        .post(&config.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body_bytes.to_vec())
        .timeout(std::time::Duration::from_secs(30));

    if let Some(ref token) = config.auth_token {
        builder = builder.header("authorization", format!("Bearer {token}"));
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

    if is_sse {
        if !(200..300).contains(&status) {
            return Err(McpError::internal(format!("HTTP {status} from MCP server")));
        }
        read_sse_event(response.into_body()).await
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
        Ok(body.to_vec())
    }
}
