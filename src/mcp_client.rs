use schemars::JsonSchema;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;

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

// ── Session cache ───────────────────────────────────────────────────────────

thread_local! {
    /// Maps session key → Mcp-Session-Id. WASM is single-threaded, RefCell is safe.
    static SESSIONS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

fn session_key(config: &Config) -> String {
    match &config.auth_token {
        Some(token) => format!("{}\0{token}", config.url),
        None => config.url.clone(),
    }
}

fn get_session(config: &Config) -> Option<String> {
    SESSIONS.with(|s| s.borrow().get(&session_key(config)).cloned())
}

fn set_session(config: &Config, session_id: String) {
    SESSIONS.with(|s| {
        s.borrow_mut().insert(session_key(config), session_id);
    });
}

fn clear_session(config: &Config) {
    SESSIONS.with(|s| {
        s.borrow_mut().remove(&session_key(config));
    });
}

// ── Config from metadata ────────────────────────────────────────────────────

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

// ── MCP protocol ────────────────────────────────────────────────────────────

/// Send initialize handshake and cache the session ID if returned.
async fn do_initialize(config: &Config) -> Result<(), McpError> {
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

    // Cache session ID if the server returned one
    if let Some(sid) = resp.session_id {
        set_session(config, sid);
    }

    // Parse result to validate the response
    let response: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| McpError::internal(format!("Invalid JSON in initialize response: {e}")))?;
    if let Some(error) = response.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("initialize failed");
        return Err(McpError::internal(msg));
    }

    // Send initialized notification (fire-and-forget)
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    });
    let notification_bytes = serde_json::to_vec(&notification)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;
    let _ = http_post(config, &notification_bytes, get_session(config)).await;

    Ok(())
}

/// Ensure we have an active session, initializing if needed.
async fn ensure_initialized(config: &Config) -> Result<(), McpError> {
    if get_session(config).is_some() {
        return Ok(());
    }
    do_initialize(config).await
}

/// Send a JSON-RPC 2.0 request to the MCP server and return the result.
///
/// Handles session lifecycle: initializes on first call, includes session ID,
/// and re-initializes on 404 (expired session).
pub async fn mcp_request(
    config: &Config,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, McpError> {
    ensure_initialized(config).await?;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))?;

    let resp = http_post(config, &body_bytes, get_session(config)).await;

    // Handle 404 — session expired, re-initialize and retry once
    let resp = match resp {
        Err(ref e) if e.message.contains("HTTP 404") => {
            clear_session(config);
            do_initialize(config).await?;
            http_post(config, &body_bytes, get_session(config)).await?
        }
        other => other?,
    };

    // Update session ID if server sent a new one
    if let Some(sid) = resp.session_id {
        set_session(config, sid);
    }

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
    session_id: Option<String>,
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
    if let Some(ref sid) = session_id {
        builder = builder.header(SESSION_HEADER, sid.as_str());
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
