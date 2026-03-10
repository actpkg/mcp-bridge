use http::Uri;
use schemars::JsonSchema;
use serde::Deserialize;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, RequestOptions, Response, Scheme};

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

/// Parse config from dCBOR bytes.
pub fn parse_config(config: Option<&[u8]>) -> Result<Config, McpError> {
    let bytes = config.ok_or_else(|| McpError::invalid_args("Config is required"))?;
    act_types::cbor::from_cbor(bytes)
        .map_err(|e| McpError::invalid_args(format!("Invalid config: {e}")))
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
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": {
            "name": "act-mcp-bridge",
            "version": "0.1.0",
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

/// Low-level HTTP POST using wasi:http.
async fn http_post(config: &Config, body_bytes: &[u8]) -> Result<Vec<u8>, McpError> {
    let uri: Uri = config
        .url
        .parse()
        .map_err(|e| McpError::invalid_args(format!("Invalid URL: {e}")))?;

    let scheme = match uri.scheme_str() {
        Some("https") => Scheme::Https,
        Some("http") => Scheme::Http,
        Some(other) => {
            return Err(McpError::invalid_args(format!(
                "Unsupported scheme: {other}"
            )))
        }
        None => return Err(McpError::invalid_args("Missing scheme in URL")),
    };

    // Build headers
    let mut header_list: Vec<(String, Vec<u8>)> = vec![
        ("content-type".to_string(), b"application/json".to_vec()),
        ("accept".to_string(), b"application/json".to_vec()),
    ];
    if let Some(ref token) = config.auth_token {
        header_list.push((
            "authorization".to_string(),
            format!("Bearer {token}").into_bytes(),
        ));
    }
    let headers =
        Fields::from_list(&header_list).map_err(|e| McpError::internal(format!("Headers error: {e:?}")))?;

    // Build request body stream
    let body_vec = body_bytes.to_vec();
    let (mut body_writer, body_reader) = wasip3::wit_stream::new::<u8>();
    wit_bindgen::spawn(async move {
        body_writer.write_all(body_vec).await;
    });

    // Trailers (none)
    let (_, trailers_reader) =
        wasip3::wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));

    // Timeout: 30s connect and first-byte
    let timeout_ns = 30_000 * 1_000_000u64; // 30s in nanoseconds
    let opts = RequestOptions::new();
    let _ = opts.set_connect_timeout(Some(timeout_ns));
    let _ = opts.set_first_byte_timeout(Some(timeout_ns));

    // Construct request
    let (request, _) = Request::new(headers, Some(body_reader), trailers_reader, Some(opts));
    let _ = request.set_method(&Method::Post);
    let _ = request.set_scheme(Some(&scheme));

    if let Some(authority) = uri.authority() {
        let _ = request.set_authority(Some(authority.as_str()));
    }

    let _ = request.set_path_with_query(uri.path_and_query().map(|pq| pq.as_str()));

    // Send request
    let response = wasip3::http::client::send(request)
        .await
        .map_err(|e| McpError::internal(format!("HTTP error: {e:?}")))?;

    // Check status code
    let status = response.get_status_code();
    if !(200..300).contains(&status) {
        return Err(McpError::internal(format!(
            "HTTP {status} from MCP server"
        )));
    }

    // Read response body
    let (_, result_reader) = wasip3::wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
    let (mut body_stream, _trailers) = Response::consume_body(response, result_reader);

    let mut all_bytes = Vec::new();
    let mut read_buf = Vec::with_capacity(16384);
    loop {
        let (result, chunk) = body_stream.read(read_buf).await;
        match result {
            wasip3::wit_bindgen::StreamResult::Complete(_) => {
                all_bytes.extend_from_slice(&chunk);
                read_buf = Vec::with_capacity(16384);
            }
            wasip3::wit_bindgen::StreamResult::Dropped
            | wasip3::wit_bindgen::StreamResult::Cancelled => break,
        }
    }

    Ok(all_bytes)
}
