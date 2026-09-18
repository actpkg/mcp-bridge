//! Streamable-HTTP client for an upstream MCP server, speaking both
//! protocol dialects that are current in the wild:
//!
//! - **legacy** (`2025-11-25` and earlier) — `initialize` /
//!   `notifications/initialized` handshake, upstream state addressed by the
//!   `Mcp-Session-Id` header, `DELETE` to tear that state down.
//! - **modern** (`2026-07-28`, SEP-2575) — no handshake and no session
//!   header. Every request is self-contained: the protocol version, client
//!   identity and client capabilities travel in `params._meta`, and the
//!   SEP-2243 `Mcp-Method` / `Mcp-Name` headers let middle boxes route
//!   without parsing the body.
//!
//! Wire types come from `rmcp::model`, which models both revisions.

use rmcp::model::{
    ClientCapabilities, ErrorCode, Implementation, ProtocolVersion, RequestMetaObject,
};

use crate::creds::{Bearer, FIELD_OAUTH, FIELD_TOKEN};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

/// Protocol revision the bridge offers when probing for the modern dialect.
const MODERN_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;
/// Protocol revision the bridge offers in the legacy `initialize` handshake.
const LEGACY_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

const CLIENT_NAME: &str = "act-mcp-bridge";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn client_identity() -> Implementation {
    Implementation::new(CLIENT_NAME, CLIENT_VERSION)
}

// ── Open-session args ──────────────────────────────────────────────────────

/// Explicit MCP protocol dialect pin for [`Config::protocol_version`].
///
/// The two values name the revision that introduced each dialect; the bridge
/// implements exactly these two wire shapes, so pinning anything else would
/// be a promise it cannot keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(crate = "schemars")]
pub enum DialectPin {
    /// `initialize` handshake plus `Mcp-Session-Id` header.
    #[serde(rename = "2025-11-25")]
    Legacy,
    /// Stateless, self-contained requests (SEP-2575 / SEP-2243).
    #[serde(rename = "2026-07-28")]
    Modern,
}

/// Per-session config: where to talk to the upstream MCP server, and which
/// credential to talk to it with.
/// Populated from `open-session.args`.
///
/// **There is no token field here, and there must never be one.** Everything
/// sent to `open-session` is agent-visible plaintext that lands in the
/// transcript and in the host's session record. The credential is *named*
/// here and fetched from the host's credential store on first use
/// (ACT-AUTH §1.1); it never crosses this boundary.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "schemars", title = "mcp-bridge open-session args")]
pub struct Config {
    /// MCP server URL (e.g. http://localhost:3000/mcp). Must not carry
    /// userinfo: a credential does not belong in a URL.
    pub url: String,
    /// Which credential in this component's profile authenticates the
    /// upstream, as an `Authorization: Bearer` header.
    ///
    /// **Optional on purpose.** A bridge legitimately points at an
    /// unauthenticated MCP server — a local one, or a public read-only
    /// endpoint — and for those there is nothing to fetch and no consent
    /// prompt to put in front of anybody. Omit it and no credential is
    /// requested at all.
    pub credential_key: Option<String>,
    /// Pin the MCP protocol revision spoken to this server. Omit to
    /// auto-detect (see [`negotiate`]).
    pub protocol_version: Option<DialectPin>,
}

impl Config {
    /// Everything that can be decided about these args without touching the
    /// network or the credential store. Run by `open-session`, so a
    /// malformed URL or key is refused before an id is issued.
    pub fn validate(&self) -> Result<(), McpError> {
        validate_url(&self.url)?;
        match &self.credential_key {
            Some(key) => crate::creds::validate_credential_key(key),
            None => Ok(()),
        }
    }

    /// What the credential is being requested *for*, as it is put in
    /// `secret-request.resource`.
    ///
    /// Scheme and authority only — no path, no query, no fragment. Every
    /// member of a `secret-request` is host-visible by contract: a host MAY
    /// show `resource` to the human it prompts and MAY record it in the audit
    /// trail. An MCP endpoint URL legitimately carries a query string, and a
    /// query string is the other common place a credential is smuggled
    /// (`?api_key=…`), so the part that could carry one is not sent. What is
    /// left is also the only part a human needs in order to answer: which
    /// server is this token for.
    pub fn resource(&self) -> String {
        origin_of(&self.url)
    }
}

/// Refuse a URL this bridge will not treat as an address.
///
/// Userinfo is a URL's spelling of a credential, and this component's whole
/// auth design exists to keep one out of session args — refusing it here is
/// the same rule as refusing an `auth_token` field, applied to the other
/// place it fits.
pub fn validate_url(url: &str) -> Result<(), McpError> {
    // RFC 3986 §3.1: the scheme is case-insensitive, and URLs get pasted out
    // of address bars. The rest is left exactly as typed — a path may
    // legitimately be case-sensitive.
    let lowered = url.to_ascii_lowercase();
    let scheme_len = if lowered.starts_with("https://") {
        "https://".len()
    } else if lowered.starts_with("http://") {
        "http://".len()
    } else {
        return Err(McpError::invalid_args(
            "url must be an http(s) URL, e.g. https://mcp.example.com/mcp",
        ));
    };

    let authority = authority_of(&url[scheme_len..]);
    if authority.contains('@') {
        return Err(McpError::invalid_args(
            "url must not carry userinfo (https://user:pass@host/...); mcp-bridge \
             authenticates from the credential store entry named by credential_key",
        ));
    }
    // The authority also has to *be* an authority. Without this,
    // `https://user:pa/ss@host/mcp` passes: RFC 3986 ends the authority at
    // the first `/`, so the `@` lands in the path where the check above never
    // sees it, while `user:pa` is kept as the host and the rest of the
    // password rides along into `resource`. A non-numeric port is the tell.
    // Neither the authority nor the URL is echoed — that is where the
    // password would be.
    if let Some(port) = port_of(authority)
        && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(McpError::invalid_args(
            "url's authority is not a host with an optional numeric port. A password \
             with a slash in it produces exactly this — check that the URL is the MCP \
             endpoint's address and nothing else.",
        ));
    }
    Ok(())
}

/// Scheme and authority of a URL already known to start with `http(s)://`.
/// Falls back to the whole string for anything else, so a caller cannot get
/// silently-empty context out of it.
fn origin_of(url: &str) -> String {
    let lowered = url.to_ascii_lowercase();
    let scheme_len = if lowered.starts_with("https://") {
        "https://".len()
    } else if lowered.starts_with("http://") {
        "http://".len()
    } else {
        return url.to_string();
    };
    format!("{}{}", &url[..scheme_len], authority_of(&url[scheme_len..]))
}

/// Everything before the first `/`, `?` or `#` — RFC 3986's authority.
fn authority_of(after_scheme: &str) -> &str {
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    &after_scheme[..end]
}

/// The port component of an authority, if it has one. `None` for a bare host
/// and for an IPv6 literal with no port after the `]`.
fn port_of(authority: &str) -> Option<&str> {
    let after_literal = authority.rfind(']').map_or(0, |i| i + 1);
    authority[after_literal..]
        .rfind(':')
        .map(|i| &authority[after_literal + i + 1..])
}

// ── Errors ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpError {
    pub kind: String,
    pub message: String,
}

/// `ACT-CONSTANTS.md` §9. Not in `act_types::constants` yet — delete this the
/// day it lands there.
pub const ERR_CREDENTIAL_REQUIRED: &str = "std:credential-required";

impl McpError {
    pub fn internal(msg: impl Into<String>) -> Self {
        McpError {
            kind: act_types::constants::ERR_INTERNAL.to_string(),
            message: msg.into(),
        }
    }

    pub fn invalid_args(msg: impl Into<String>) -> Self {
        McpError {
            kind: act_types::constants::ERR_INVALID_ARGS.to_string(),
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        McpError {
            kind: act_types::constants::ERR_NOT_FOUND.to_string(),
            message: msg.into(),
        }
    }

    /// The upstream refused what this bridge sent it, or the host refused to
    /// hand over the credential. Distinct from every other kind because it is
    /// the one the session is poisoned on: see `lib.rs`.
    pub fn capability_denied(msg: impl Into<String>) -> Self {
        McpError {
            kind: act_types::constants::ERR_CAPABILITY_DENIED.to_string(),
            message: msg.into(),
        }
    }

    /// A credential the component needs is absent, unreadable, or expired.
    /// The fix is a command an operator runs, and every message carrying this
    /// kind names it (`ACT-CONSTANTS.md` §9).
    pub fn credential_required(msg: impl Into<String>) -> Self {
        McpError {
            kind: ERR_CREDENTIAL_REQUIRED.to_string(),
            message: msg.into(),
        }
    }

    /// True for the failures that cannot change within one session: the
    /// credential was refused, or is not usable as stored. Retrying either
    /// means calling `get-secret` again, which can put a consent prompt in
    /// front of a human on every single tool call.
    pub fn poisons_the_session(&self) -> bool {
        self.kind == act_types::constants::ERR_CAPABILITY_DENIED
            || self.kind == ERR_CREDENTIAL_REQUIRED
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Map a JSON-RPC error object onto an ACT error kind.
///
/// `-32602` is overloaded since SEP-2164: a `2026-07-28` server reports "the
/// thing you named does not exist" as Invalid Params instead of the pre-2026
/// `-32002`, so the code alone no longer separates bad arguments from a
/// missing resource — and nothing survives the rewrite to discriminate them
/// (the server simply swaps the code, leaving message and `data` untouched).
/// A `-32602` is therefore classified as `std:not-found` only on a positive
/// signal — a `uri` in `data`, or a not-found phrase in the message — and
/// falls back to `std:invalid-args` otherwise.
fn map_jsonrpc_error(code: i64, message: &str, data: Option<&Value>) -> McpError {
    match ErrorCode(i32::try_from(code).unwrap_or(i32::MIN)) {
        ErrorCode::RESOURCE_NOT_FOUND | ErrorCode::METHOD_NOT_FOUND => McpError::not_found(message),
        ErrorCode::INVALID_PARAMS if looks_like_not_found(message, data) => {
            McpError::not_found(message)
        }
        ErrorCode::INVALID_PARAMS | ErrorCode::INVALID_REQUEST => McpError::invalid_args(message),
        // These three mean the bridge and the upstream disagree about the
        // dialect itself, which is a bridge-side problem, not a caller one.
        ErrorCode::HEADER_MISMATCH
        | ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY
        | ErrorCode::UNSUPPORTED_PROTOCOL_VERSION => McpError::internal(format!(
            "MCP protocol negotiation error ({code}): {message}"
        )),
        _ => McpError::internal(message),
    }
}

fn looks_like_not_found(message: &str, data: Option<&Value>) -> bool {
    if data.and_then(|d| d.get("uri")).is_some() {
        return true;
    }
    let lowered = message.to_ascii_lowercase();
    [
        "not found",
        "does not exist",
        "doesn't exist",
        "no such",
        "unknown tool",
        "unknown prompt",
        "unknown resource",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

// ── Dialect ────────────────────────────────────────────────────────────────

/// The wire shape negotiated with one upstream server, held for the lifetime
/// of the ACT session that opened it.
#[derive(Debug, Clone)]
pub enum Dialect {
    Legacy {
        /// Revision echoed by the upstream `initialize` result.
        version: ProtocolVersion,
        /// `Mcp-Session-Id` the upstream issued, when it issued one.
        /// Stateless legacy servers leave this `None`.
        session_id: Option<String>,
    },
    Modern {
        version: ProtocolVersion,
    },
}

impl Dialect {
    fn version(&self) -> &ProtocolVersion {
        match self {
            Dialect::Legacy { version, .. } | Dialect::Modern { version } => version,
        }
    }
}

/// Establish which MCP dialect the upstream speaks.
///
/// Detection rule when `Config::protocol_version` is absent (auto mode):
///
/// 1. POST `server/discover` (SEP-2575) carrying
///    `_meta."io.modelcontextprotocol/protocolVersion" = "2026-07-28"`.
/// 2. A result whose `supportedVersions` contains `2026-07-28` means the
///    upstream is modern: no handshake runs and no session header is ever
///    sent.
/// 3. **Any** other outcome counts as "not modern" and the legacy
///    `initialize` handshake runs: a JSON-RPC error (`-32601` from a
///    pre-SEP-2575 server, `-32022` unsupported-version, …), a non-2xx status
///    (older Streamable-HTTP servers answer a pre-`initialize` POST with 400),
///    an unparseable body, or a `supportedVersions` list without
///    `2026-07-28`. Being this permissive costs one wasted round trip against
///    a legacy server but keeps detection working against implementations
///    that reject unknown methods at the HTTP layer rather than in JSON-RPC.
/// 4. A transport or auth failure therefore also falls through to the
///    handshake, which fails the same way — so the handshake's error is
///    returned, with the probe's failure appended so the real cause is not
///    masked by the fallback.
///
/// A pinned `protocol_version` skips probe-and-fall-back entirely: the named
/// dialect is used, and a mismatch surfaces as an error rather than a silent
/// downgrade.
pub async fn negotiate(config: &Config, auth: Option<&Bearer>) -> Result<Dialect, McpError> {
    match config.protocol_version {
        // A pinned dialect keeps the upstream's own error kind — a 401 is
        // still a 401 — but names the pin, so the operator can tell that no
        // fallback was attempted.
        Some(DialectPin::Modern) => discover_modern(config, auth).await.map_err(|e| McpError {
            kind: e.kind,
            message: format!(
                "protocol_version is pinned to MCP {}: {}",
                MODERN_VERSION.as_str(),
                e.message
            ),
        }),
        Some(DialectPin::Legacy) => legacy_handshake(config, auth).await.map_err(|e| McpError {
            kind: e.kind,
            message: format!(
                "protocol_version is pinned to MCP {}: {}",
                LEGACY_VERSION.as_str(),
                e.message
            ),
        }),
        None => match discover_modern(config, auth).await {
            Ok(dialect) => Ok(dialect),
            Err(probe) => legacy_handshake(config, auth).await.map_err(|e| McpError {
                kind: e.kind,
                message: format!(
                    "{} (the MCP {} probe first failed with: {})",
                    e.message,
                    MODERN_VERSION.as_str(),
                    probe.message
                ),
            }),
        },
    }
}

/// Probe for the modern dialect with `server/discover`.
async fn discover_modern(config: &Config, auth: Option<&Bearer>) -> Result<Dialect, McpError> {
    let dialect = Dialect::Modern {
        version: MODERN_VERSION,
    };
    let result = mcp_request(config, auth, &dialect, "server/discover", json!({})).await?;

    let supported = result.get("supportedVersions").and_then(Value::as_array);
    let advertised = supported.is_some_and(|versions| {
        versions
            .iter()
            .any(|v| v.as_str() == Some(MODERN_VERSION.as_str()))
    });
    if !advertised {
        return Err(McpError::internal(format!(
            "server/discover did not advertise MCP {}; supportedVersions = {}",
            MODERN_VERSION.as_str(),
            supported.map_or_else(
                || "absent".to_string(),
                |v| Value::Array(v.clone()).to_string()
            ),
        )));
    }
    Ok(dialect)
}

/// Run the legacy `initialize` + `notifications/initialized` handshake and
/// capture the upstream `Mcp-Session-Id`, if the server issues one.
async fn legacy_handshake(config: &Config, auth: Option<&Bearer>) -> Result<Dialect, McpError> {
    let params = json!({
        "protocolVersion": LEGACY_VERSION.as_str(),
        "capabilities": ClientCapabilities::default(),
        "clientInfo": client_identity(),
    });
    // The `MCP-Protocol-Version` header only becomes mandatory *after*
    // initialization, and strict servers reject a header that disagrees with
    // the negotiated version — so the handshake itself goes out bare.
    let resp = post(
        config,
        auth,
        &to_body(&envelope("initialize", &params))?,
        &[],
    )
    .await?;
    let result = parse_jsonrpc(&resp.body)?;

    let dialect = Dialect::Legacy {
        version: result
            .get("protocolVersion")
            .and_then(|v| serde_json::from_value::<ProtocolVersion>(v.clone()).ok())
            .unwrap_or(LEGACY_VERSION),
        session_id: resp.session_id,
    };

    // Fire-and-forget: a server that ignores the notification is still usable.
    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    if let Ok(body) = to_body(&notification) {
        let headers = request_headers(&dialect, "notifications/initialized", &Value::Null);
        let _ = post(config, auth, &body, &headers).await;
    }

    Ok(dialect)
}

/// Shape the params and headers of one request for the negotiated dialect.
fn prepare_request(
    dialect: &Dialect,
    method: &str,
    mut params: Value,
) -> (Value, Vec<(String, String)>) {
    if let Dialect::Modern { version } = dialect {
        // SEP-2575: with the handshake gone, every request carries the client
        // context that `initialize` used to establish once.
        inject_client_meta(&mut params, version);
    }
    let headers = request_headers(dialect, method, &params);
    (params, headers)
}

/// Send a JSON-RPC 2.0 request to the upstream in the negotiated dialect.
pub async fn mcp_request(
    config: &Config,
    auth: Option<&Bearer>,
    dialect: &Dialect,
    method: &str,
    params: Value,
) -> Result<Value, McpError> {
    let (params, headers) = prepare_request(dialect, method, params);
    let resp = post(
        config,
        auth,
        &to_body(&envelope(method, &params))?,
        &headers,
    )
    .await?;
    parse_jsonrpc(&resp.body)
}

/// Best-effort upstream teardown. Only the legacy dialect has anything to
/// tear down — a modern server holds no per-client state to release. Cleanup
/// errors are swallowed by design: `close-session` is advisory in WIT.
pub async fn close_upstream(config: &Config, auth: Option<&Bearer>, dialect: &Dialect) {
    let Dialect::Legacy {
        version,
        session_id: Some(session_id),
    } = dialect
    else {
        return;
    };

    let Ok(client) = client() else { return };
    let mut builder = client
        .delete(&config.url)
        .header("accept", "application/json")
        .header(SESSION_HEADER, session_id)
        .header(PROTOCOL_VERSION_HEADER, version.as_str())
        .timeouts(timeouts(5));
    if let Some(auth) = auth {
        builder = builder.header("authorization", &auth.header_value());
    }
    let _ = builder.send().await;
}

// ── JSON-RPC envelope ──────────────────────────────────────────────────────

fn envelope(method: &str, params: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
}

fn to_body(value: &Value) -> Result<Vec<u8>, McpError> {
    serde_json::to_vec(value).map_err(|e| McpError::internal(format!("JSON serialize error: {e}")))
}

fn parse_jsonrpc(body: &[u8]) -> Result<Value, McpError> {
    let response: Value = serde_json::from_slice(body)
        .map_err(|e| McpError::internal(format!("Invalid JSON response: {e}")))?;

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error");
        return Err(map_jsonrpc_error(code, message, error.get("data")));
    }

    response
        .get("result")
        .cloned()
        .ok_or_else(|| McpError::internal("JSON-RPC response missing 'result' field"))
}

/// Write the SEP-2575 client context into `params._meta`.
fn inject_client_meta(params: &mut Value, version: &ProtocolVersion) {
    let meta = RequestMetaObject::with_client_context(
        version.clone(),
        client_identity(),
        ClientCapabilities::default(),
    );
    let meta = serde_json::to_value(&meta).unwrap_or_else(|_| json!({}));
    match params {
        Value::Object(map) => {
            map.insert("_meta".to_string(), meta);
        }
        other => *other = json!({ "_meta": meta }),
    }
}

// ── HTTP headers ───────────────────────────────────────────────────────────

const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
// SEP-2243 routing headers, sent only in the modern dialect.
const HEADER_MCP_METHOD: &str = "mcp-method";
const HEADER_MCP_NAME: &str = "mcp-name";
/// Sentinel wrapping a Base64-encoded SEP-2243 header value.
const BASE64_HEADER_PREFIX: &str = "=?base64?";
const BASE64_HEADER_SUFFIX: &str = "?=";

/// Methods whose `Mcp-Name` is sourced from `params.name`.
const NAME_FROM_NAME: &[&str] = &["tools/call", "prompts/get"];
/// Methods whose `Mcp-Name` is sourced from `params.uri`.
const NAME_FROM_URI: &[&str] = &[
    "resources/read",
    "resources/subscribe",
    "resources/unsubscribe",
];
/// Methods whose `Mcp-Name` is sourced from `params.taskId` (SEP-2663).
const NAME_FROM_TASK_ID: &[&str] = &["tasks/get", "tasks/update", "tasks/cancel"];

/// Headers for one request in the negotiated dialect.
///
/// Legacy requests carry the upstream session header (when the server issued
/// one); modern requests carry the SEP-2243 routing headers instead — there
/// is no session to name.
///
/// `Mcp-Param-*` promotion (the `x-mcp-header` schema annotation) is **not**
/// implemented: it needs the called tool's input schema, which the bridge
/// does not cache. A server whose tools use `x-mcp-header` will reject those
/// calls with `-32020`. See the report / SKILL.md limitations.
fn request_headers(dialect: &Dialect, method: &str, params: &Value) -> Vec<(String, String)> {
    let mut headers = vec![(
        PROTOCOL_VERSION_HEADER.to_string(),
        dialect.version().as_str().to_string(),
    )];
    match dialect {
        Dialect::Legacy { session_id, .. } => {
            if let Some(session_id) = session_id {
                headers.push((SESSION_HEADER.to_string(), session_id.clone()));
            }
        }
        Dialect::Modern { .. } => {
            headers.push((HEADER_MCP_METHOD.to_string(), method.to_string()));
            if let Some(name) = mcp_name(method, params) {
                headers.push((HEADER_MCP_NAME.to_string(), encode_header_value(&name)));
            }
        }
    }
    headers
}

/// The `Mcp-Name` value for a request, if the method carries one.
fn mcp_name(method: &str, params: &Value) -> Option<String> {
    let key = if NAME_FROM_NAME.contains(&method) {
        "name"
    } else if NAME_FROM_URI.contains(&method) {
        "uri"
    } else if NAME_FROM_TASK_ID.contains(&method) {
        "taskId"
    } else {
        return None;
    };
    params.get(key)?.as_str().map(str::to_owned)
}

/// True if `value` cannot travel as a bare HTTP header value: leading or
/// trailing space/tab, control or non-ASCII characters, or a value that
/// already looks like the Base64 sentinel.
fn requires_base64(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let bytes = value.as_bytes();
    if matches!(bytes.first(), Some(b' ' | b'\t')) || matches!(bytes.last(), Some(b' ' | b'\t')) {
        return true;
    }
    if value
        .chars()
        .any(|c| (c as u32) < 0x20 || (c as u32) > 0x7E)
    {
        return true;
    }
    value.starts_with(BASE64_HEADER_PREFIX) && value.ends_with(BASE64_HEADER_SUFFIX)
}

/// Wrap a value as `=?base64?<b64>?=` when it cannot travel verbatim. Also
/// the reason a tool name containing CR/LF cannot inject a header.
fn encode_header_value(value: &str) -> String {
    use base64::Engine;
    if requires_base64(value) {
        format!(
            "{BASE64_HEADER_PREFIX}{}{BASE64_HEADER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    } else {
        value.to_owned()
    }
}

// ── HTTP transport ──────────────────────────────────────────────────────────

struct HttpResponse {
    body: Vec<u8>,
    session_id: Option<String>,
}

/// The error for an upstream status that means "not with that credential",
/// or `None` for every other status.
///
/// Separated from the rest of the status handling for two reasons. It is the
/// one refusal that poisons the session — `lib.rs` keys that decision off
/// `McpError::poisons_the_session`, so a 401 must not arrive as a generic
/// `std:internal` — and it is the one whose fix depends on what the session
/// was opened with, which is why it takes the config.
///
/// **The response body is deliberately not quoted.** It is the answer to a
/// request that carried an `Authorization` header, and a debug-happy upstream
/// echoes headers back. Nothing in a 401 body is worth the chance of putting
/// the token in the agent's transcript.
fn credential_refusal(status: u16, config: &Config) -> Option<McpError> {
    if status != 401 && status != 403 {
        return None;
    }
    Some(McpError::capability_denied(match &config.credential_key {
        Some(key) => format!(
            "The upstream MCP server refused this bridge's credential (HTTP {status}). The \
             credential under key '{key}' is wrong, expired, or lacks the scope this server \
             wants. Re-provision it with `act secret set <component-ref> --key {key} --field \
             {FIELD_TOKEN} --fields-stdin` (or `act login <component-ref> --key {key} --field \
             {FIELD_OAUTH} --force` for an OAuth one) and open a new session — this one will \
             not retry."
        ),
        None => format!(
            "The upstream MCP server requires authentication (HTTP {status}) and this session \
             named no credential. Re-open it with `credential_key` set, having stored a token \
             under that key with `act secret set <component-ref> --key <key> --field \
             {FIELD_TOKEN} --fields-stdin`."
        ),
    }))
}

/// Read an SSE response until the first event carrying data, and stop there.
///
/// The decoding is `hclient`'s, not ours: `SseStream` implements the WHATWG
/// event-stream grammar (multi-line `data:`, CR/CRLF/LF, chunk boundaries
/// that fall anywhere) and bounds a single event at `MAX_RESPONSE_BYTES`.
/// It reads only as many body chunks as the first event needs, which is what
/// this transport wants: a JSON-RPC response is one event, and the stream it
/// arrives on may never be closed by the server.
///
/// `Comment` and `Retry` events are skipped rather than treated as the
/// answer — a keep-alive comment is exactly what an upstream sends while it
/// is still working on the response.
async fn read_sse_event(
    response: hclient::Response<hclient::body::ClientBody>,
) -> Result<Vec<u8>, McpError> {
    let mut stream = hclient::sse::SseStream::new(response, MAX_RESPONSE_BYTES)
        .map_err(|e| McpError::internal(format!("Invalid SSE response from MCP server: {e}")))?;

    while let Some(event) = stream.next().await {
        let event = event
            .map_err(|e| McpError::internal(format!("Cannot read the MCP SSE response: {e}")))?;
        if let hclient::sse::SseEvent::Message { data, .. } = event
            && !data.is_empty()
        {
            return Ok(data.into_bytes());
        }
    }
    Err(McpError::internal("SSE stream ended without a data event"))
}

/// The HTTP client for one request.
///
/// Built per call rather than cached: `Client` is not `Sync`, the component
/// is single-threaded, and construction is cheap next to a network round
/// trip — a connection pool shared across sessions would also mean one
/// session's upstream connection serving another session's credential.
fn client() -> Result<hclient::Client, McpError> {
    hclient::Client::builder(hclient_wasi::WasiHttp::new())
        .build()
        .map_err(|e| McpError::internal(format!("Cannot build the HTTP client: {e}")))
}

/// Timeouts for a request whose whole response is read here.
///
/// `Timeouts` is `#[non_exhaustive]`, so it is built by `with_*` rather than
/// a struct literal; each field left unset is unbounded, which is why both
/// legs are named explicitly.
fn timeouts(secs: u64) -> hclient::Timeouts {
    let d = std::time::Duration::from_secs(secs);
    hclient::Timeouts::new().with_connect(d).with_first_byte(d)
}

/// Low-level HTTP POST (Streamable HTTP transport).
async fn post(
    config: &Config,
    auth: Option<&Bearer>,
    body_bytes: &[u8],
    headers: &[(String, String)],
) -> Result<HttpResponse, McpError> {
    let client = client()?;
    let mut builder = client
        .post(&config.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(hclient::RequestBody::Full(bytes::Bytes::copy_from_slice(
            body_bytes,
        )))
        // Deliberately no `between_bytes`: on the SSE leg an upstream may
        // legitimately stay silent between events, and bounding the gap
        // would kill a healthy stream. `first_byte` bounds the wait for the
        // response head, which is the failure worth catching.
        .timeouts(timeouts(30));

    if let Some(auth) = auth {
        builder = builder.header("authorization", &auth.header_value());
    }
    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    let response = builder
        .send()
        .await
        .map_err(|e| McpError::internal(format!("HTTP error: {e}")))?;

    // Both are read before anything consumes the response: `SseStream::new`
    // takes it by value.
    let status = response.status().as_u16();
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_event_stream);
    let resp_session_id = response
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(refusal) = credential_refusal(status, config) {
        return Err(refusal);
    }

    if is_sse {
        if !(200..300).contains(&status) {
            return Err(McpError::internal(format!("HTTP {status} from MCP server")));
        }
        Ok(HttpResponse {
            body: read_sse_event(response).await?,
            session_id: resp_session_id,
        })
    } else {
        let body = response
            .collect()
            .await
            .map_err(|e| McpError::internal(format!("Cannot read the MCP response: {e}")))?;
        let body = body.bytes();
        if !(200..300).contains(&status) {
            let detail = String::from_utf8_lossy(body);
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

/// Whether a `Content-Type` names the SSE media type.
///
/// A prefix test would accept `text/event-streamish`, so the media type is
/// matched to its delimiter. Parameters (`; charset=utf-8`) are allowed and
/// the comparison is case-insensitive, per RFC 9110 §8.3.
fn is_event_stream(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_ascii();
    media_type.eq_ignore_ascii_case("text/event-stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn legacy(session_id: Option<&str>) -> Dialect {
        Dialect::Legacy {
            version: LEGACY_VERSION,
            session_id: session_id.map(String::from),
        }
    }

    fn modern() -> Dialect {
        Dialect::Modern {
            version: MODERN_VERSION,
        }
    }

    // ── open-session args ──────────────────────────────────────────────

    /// Collect every property name in a schema, at any depth.
    fn property_names(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    if k == "properties"
                        && let Some(props) = v.as_object()
                    {
                        out.extend(props.keys().cloned());
                    }
                    property_names(v, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| property_names(v, out)),
            _ => {}
        }
    }

    /// The security property the whole credential design exists to protect,
    /// asserted rather than eyeballed: **there must be nowhere in the
    /// open-args schema to put a token.** Everything sent to `open-session`
    /// is agent-visible plaintext that lands in the transcript and in the
    /// host's session record, and this component used to take an
    /// `auth_token` there.
    ///
    /// The root fields are an **allowlist**, spelled out exactly. A denylist
    /// of secret-sounding words cannot do this job on its own — the field
    /// this replaced was called `auth_token`, but the next one could be
    /// `bearer` or `key`. Having to edit this list when a field is added is
    /// the point: adding one is a decision about what an agent may hand over.
    ///
    /// The word search still runs, over field names at any depth, for the one
    /// thing the allowlist cannot see: a secret hidden inside a nested object
    /// or a `$defs` entry. It reads names, not the raw document, because the
    /// descriptions deliberately use the words "token" and "credential" to
    /// tell the agent where the credential really comes from — searching the
    /// whole schema would fail on its own warning.
    #[test]
    fn the_open_args_schema_names_a_credential_but_never_carries_one() {
        let json = serde_json::to_value(schemars::schema_for!(Config)).expect("serializes");

        let mut root: Vec<&str> = json["properties"]
            .as_object()
            .expect("an object schema")
            .keys()
            .map(String::as_str)
            .collect();
        root.sort_unstable();
        assert_eq!(root, ["credential_key", "protocol_version", "url"]);

        let mut names = Vec::new();
        property_names(&json, &mut names);
        assert!(names.iter().any(|n| n == "credential_key"), "{names:?}");
        for secret in [
            "password", "passwd", "pwd", "secret", "token", "auth", "api_key",
        ] {
            assert!(
                !names.iter().any(|n| n.to_lowercase().contains(secret)),
                "open-args schema offers a place to put a {secret}: {names:?}"
            );
        }
    }

    #[test]
    fn the_credential_key_is_optional_so_an_unauthenticated_upstream_needs_nothing() {
        let config: Config = serde_json::from_value(json!({ "url": "http://x/mcp" })).unwrap();
        assert_eq!(config.credential_key, None);
        assert!(config.validate().is_ok());
    }

    // ── url guards ─────────────────────────────────────────────────────

    #[test]
    fn a_url_must_be_http_or_https() {
        assert!(validate_url("https://h/mcp").is_ok());
        assert!(validate_url("HTTP://h/mcp").is_ok(), "RFC 3986 §3.1");
        assert!(validate_url("ftp://h/mcp").is_err());
        assert!(validate_url("h/mcp").is_err(), "not a relative URL");
    }

    /// Userinfo is a URL's spelling of a credential, and `url` is also what
    /// travels to the host in `secret-request.resource`.
    #[test]
    fn a_url_carrying_userinfo_is_refused_and_never_echoed() {
        let e = validate_url("https://user:hunter2-sentinel@h/mcp").expect_err("refused");
        assert_eq!(e.kind, "std:invalid-args");
        assert!(e.message.contains("credential_key"), "{}", e.message);
        assert!(!e.message.contains("hunter2-sentinel"), "{}", e.message);
    }

    /// The way past the check above: a `/` inside the password ends the
    /// authority early, so the `@` lands in the path. `user:pa` is then kept
    /// as the host and the rest rides along. A non-numeric port is the tell.
    #[test]
    fn a_password_with_a_slash_in_it_does_not_slip_through_as_a_path() {
        let e = validate_url("https://user:pa/ss@h/mcp").expect_err("refused");
        assert_eq!(e.kind, "std:invalid-args");
        assert!(!e.message.contains("ss@h"), "{}", e.message);
        // A real port is still fine, and so is an IPv6 literal.
        assert!(validate_url("http://127.0.0.1:3000/mcp").is_ok());
        assert!(validate_url("http://[::1]:3000/mcp").is_ok());
        assert!(validate_url("http://[::1]/mcp").is_ok());
    }

    /// `resource` is host-visible by contract — a host MAY show it to the
    /// human it prompts and MAY record it — so the two parts of a URL that
    /// can carry a credential never reach it. Userinfo is refused outright;
    /// a query string is legal on an MCP endpoint, so it is dropped instead.
    #[test]
    fn the_secret_request_resource_is_the_origin_and_nothing_else() {
        let with_query: Config = serde_json::from_value(
            json!({ "url": "https://mcp.example.com:8443/mcp?api_key=hunter2-sentinel" }),
        )
        .unwrap();
        assert_eq!(with_query.resource(), "https://mcp.example.com:8443");

        let plain: Config =
            serde_json::from_value(json!({ "url": "http://127.0.0.1:3000/mcp" })).unwrap();
        assert_eq!(plain.resource(), "http://127.0.0.1:3000");
    }

    // ── upstream refusals ──────────────────────────────────────────────

    /// A 401 must not arrive as `std:internal`: `lib.rs` poisons the session
    /// on `std:capability-denied` and on nothing else, so the kind is what
    /// stops a rejected credential being re-fetched on every call.
    #[test]
    fn a_401_or_403_is_a_capability_denial_that_poisons_the_session() {
        let cfg: Config =
            serde_json::from_value(json!({ "url": "https://h/mcp", "credential_key": "prod" }))
                .unwrap();
        for status in [401u16, 403] {
            let e = credential_refusal(status, &cfg).expect("a credential refusal");
            assert_eq!(e.kind, "std:capability-denied");
            assert!(e.poisons_the_session());
            assert!(e.message.contains("--key prod"), "{}", e.message);
            assert!(e.message.contains(FIELD_TOKEN), "{}", e.message);
        }
        assert!(credential_refusal(500, &cfg).is_none());
        assert!(credential_refusal(200, &cfg).is_none());
    }

    /// The other half: a session that named no credential gets told to name
    /// one, not to re-provision a key it never used.
    #[test]
    fn a_401_without_a_credential_key_says_to_name_one() {
        let cfg: Config = serde_json::from_value(json!({ "url": "https://h/mcp" })).unwrap();
        let e = credential_refusal(401, &cfg).expect("a credential refusal");
        assert!(e.message.contains("credential_key"), "{}", e.message);
    }

    #[test]
    fn config_schema_offers_both_dialects() {
        let schema = serde_json::to_string(&schemars::schema_for!(Config)).unwrap();
        assert!(schema.contains("2025-11-25"), "{schema}");
        assert!(schema.contains("2026-07-28"), "{schema}");
        assert!(schema.contains("protocol_version"), "{schema}");
    }

    #[test]
    fn protocol_version_is_optional() {
        let config: Config = serde_json::from_value(json!({ "url": "http://x/mcp" })).unwrap();
        assert_eq!(config.protocol_version, None);
    }

    #[test]
    fn dialect_pin_deserializes_from_revision_strings() {
        let config: Config = serde_json::from_value(
            json!({ "url": "http://x/mcp", "protocol_version": "2026-07-28" }),
        )
        .unwrap();
        assert_eq!(config.protocol_version, Some(DialectPin::Modern));

        let config: Config = serde_json::from_value(
            json!({ "url": "http://x/mcp", "protocol_version": "2025-11-25" }),
        )
        .unwrap();
        assert_eq!(config.protocol_version, Some(DialectPin::Legacy));
    }

    #[test]
    fn unknown_dialect_pin_is_rejected() {
        let parsed = serde_json::from_value::<Config>(
            json!({ "url": "http://x/mcp", "protocol_version": "2024-11-05" }),
        );
        assert!(parsed.is_err());
    }

    // ── legacy dialect ─────────────────────────────────────────────────

    #[test]
    fn legacy_sends_session_header_and_no_routing_headers() {
        let params = json!({ "name": "echo", "arguments": {} });
        let headers = request_headers(&legacy(Some("sid-42")), "tools/call", &params);

        assert_eq!(header(&headers, SESSION_HEADER), Some("sid-42"));
        assert_eq!(
            header(&headers, PROTOCOL_VERSION_HEADER),
            Some("2025-11-25")
        );
        assert_eq!(header(&headers, HEADER_MCP_METHOD), None);
        assert_eq!(header(&headers, HEADER_MCP_NAME), None);
    }

    #[test]
    fn legacy_without_upstream_session_omits_the_header() {
        let headers = request_headers(&legacy(None), "tools/list", &json!({}));
        assert_eq!(header(&headers, SESSION_HEADER), None);
    }

    #[test]
    fn legacy_params_carry_no_client_meta() {
        // The legacy handshake establishes the client context once, so
        // nothing is injected per request.
        let (params, _) = prepare_request(
            &legacy(Some("sid-42")),
            "tools/call",
            json!({ "name": "echo", "arguments": {} }),
        );
        assert!(params.get("_meta").is_none(), "{params}");
    }

    // ── modern dialect ─────────────────────────────────────────────────

    #[test]
    fn modern_sends_routing_headers_and_no_session_header() {
        let params = json!({ "name": "echo", "arguments": {} });
        let headers = request_headers(&modern(), "tools/call", &params);

        assert_eq!(header(&headers, HEADER_MCP_METHOD), Some("tools/call"));
        assert_eq!(header(&headers, HEADER_MCP_NAME), Some("echo"));
        assert_eq!(
            header(&headers, PROTOCOL_VERSION_HEADER),
            Some("2026-07-28")
        );
        assert_eq!(header(&headers, SESSION_HEADER), None);
    }

    #[test]
    fn modern_params_carry_client_meta_on_every_request() {
        let (params, _) = prepare_request(&modern(), "tools/list", json!({}));
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/protocolVersion"],
            json!("2026-07-28")
        );
    }

    #[test]
    fn modern_omits_name_for_methods_without_one() {
        let headers = request_headers(&modern(), "tools/list", &json!({}));
        assert_eq!(header(&headers, HEADER_MCP_METHOD), Some("tools/list"));
        assert_eq!(header(&headers, HEADER_MCP_NAME), None);

        let headers = request_headers(&modern(), "server/discover", &json!({}));
        assert_eq!(header(&headers, HEADER_MCP_METHOD), Some("server/discover"));
        assert_eq!(header(&headers, HEADER_MCP_NAME), None);
    }

    #[test]
    fn mcp_name_sources_match_the_method_table() {
        assert_eq!(
            mcp_name("prompts/get", &json!({ "name": "greet" })).as_deref(),
            Some("greet")
        );
        assert_eq!(
            mcp_name("resources/read", &json!({ "uri": "file:///x" })).as_deref(),
            Some("file:///x")
        );
        assert_eq!(
            mcp_name("tasks/get", &json!({ "taskId": "t-1" })).as_deref(),
            Some("t-1")
        );
        assert_eq!(mcp_name("ping", &json!({ "name": "echo" })), None);
    }

    #[test]
    fn header_values_survive_hostile_tool_names() {
        assert_eq!(encode_header_value("echo"), "echo");
        assert_eq!(encode_header_value("a b c"), "a b c");
        for hostile in ["a\r\nEvil: 1", "café", " padded", "trailing "] {
            let encoded = encode_header_value(hostile);
            assert!(encoded.starts_with(BASE64_HEADER_PREFIX), "{encoded}");
            assert!(encoded.ends_with(BASE64_HEADER_SUFFIX), "{encoded}");
        }
    }

    #[test]
    fn client_meta_carries_the_sep_2575_keys() {
        let mut params = json!({ "name": "echo" });
        inject_client_meta(&mut params, &MODERN_VERSION);

        let meta = &params["_meta"];
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            json!("2026-07-28")
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["name"],
            json!(CLIENT_NAME)
        );
        assert!(
            meta.get("io.modelcontextprotocol/clientCapabilities")
                .is_some()
        );
        // Existing params are preserved alongside `_meta`.
        assert_eq!(params["name"], json!("echo"));
    }

    #[test]
    fn client_meta_replaces_non_object_params() {
        let mut params = Value::Null;
        inject_client_meta(&mut params, &MODERN_VERSION);
        assert!(params["_meta"]["io.modelcontextprotocol/clientInfo"].is_object());
    }

    // ── JSON-RPC error mapping ─────────────────────────────────────────

    #[test]
    fn method_not_found_maps_to_not_found() {
        assert_eq!(
            map_jsonrpc_error(-32601, "Method not found", None).kind,
            "std:not-found"
        );
    }

    #[test]
    fn legacy_resource_not_found_code_maps_to_not_found() {
        assert_eq!(
            map_jsonrpc_error(-32002, "Resource missing", None).kind,
            "std:not-found"
        );
    }

    #[test]
    fn invalid_params_defaults_to_invalid_args() {
        assert_eq!(
            map_jsonrpc_error(-32602, "city must be a string", None).kind,
            "std:invalid-args"
        );
        assert_eq!(
            map_jsonrpc_error(-32600, "malformed request", None).kind,
            "std:invalid-args"
        );
    }

    #[test]
    fn invalid_params_with_not_found_signal_maps_to_not_found() {
        // SEP-2164 moved resource-not-found onto -32602.
        assert_eq!(
            map_jsonrpc_error(-32602, "Resource not found", None).kind,
            "std:not-found"
        );
        assert_eq!(
            map_jsonrpc_error(-32602, "Unknown tool: frobnicate", None).kind,
            "std:not-found"
        );
        assert_eq!(
            map_jsonrpc_error(-32602, "bad params", Some(&json!({ "uri": "file:///x" }))).kind,
            "std:not-found"
        );
    }

    #[test]
    fn negotiation_codes_map_to_internal_and_name_the_code() {
        for code in [-32020, -32021, -32022] {
            let err = map_jsonrpc_error(code, "nope", None);
            assert_eq!(err.kind, "std:internal");
            assert!(err.message.contains(&code.to_string()), "{}", err.message);
        }
    }

    #[test]
    fn unknown_codes_map_to_internal() {
        assert_eq!(map_jsonrpc_error(-32603, "boom", None).kind, "std:internal");
    }

    // ── JSON-RPC envelope ──────────────────────────────────────────────

    #[test]
    fn jsonrpc_error_bodies_become_typed_errors() {
        let body = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#;
        let err = parse_jsonrpc(body).unwrap_err();
        assert_eq!(err.kind, "std:not-found");
        assert_eq!(err.message, "nope");
    }

    #[test]
    fn jsonrpc_results_pass_through() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        assert_eq!(parse_jsonrpc(body).unwrap(), json!({ "tools": [] }));
    }

    // ── SSE content-type sniffing ──────────────────────────────────────
    //
    // Which branch `post` takes hangs entirely on this predicate: a false
    // negative reads an event stream as if it were a JSON body, and a false
    // positive hands `SseStream::new` something it will reject as a decode
    // error. The SSE decoding itself is `hclient`'s and tested there; this
    // is the part that is ours.

    #[test]
    fn the_sse_media_type_is_recognised_with_and_without_parameters() {
        assert!(is_event_stream("text/event-stream"));
        assert!(is_event_stream("text/event-stream; charset=utf-8"));
        assert!(is_event_stream("text/event-stream;charset=utf-8"));
        // RFC 9110 §8.3: the media type is case-insensitive, and a server
        // may pad around the parameter delimiter.
        assert!(is_event_stream("Text/Event-Stream"));
        assert!(is_event_stream("  text/event-stream  "));
    }

    #[test]
    fn a_media_type_that_merely_starts_with_the_sse_one_is_not_sse() {
        // The predicate this replaced was `starts_with`, which took these.
        assert!(!is_event_stream("text/event-streamish"));
        assert!(!is_event_stream("text/event-stream-v2"));
    }

    #[test]
    fn json_is_not_sse() {
        assert!(!is_event_stream("application/json"));
        assert!(!is_event_stream("application/json; charset=utf-8"));
        assert!(!is_event_stream(""));
    }
}
