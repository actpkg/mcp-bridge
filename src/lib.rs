//! mcp-bridge — proxy a remote MCP server's tools as local ACT tools.
//!
//! Each `open-session` negotiates a protocol dialect with the upstream
//! (see `mcp_client::negotiate`) and holds the result for the lifetime of
//! the bridge-issued session-id. Subsequent capability calls reference the
//! bridge id via `std:session-id`.
//!
//! Against a **legacy** (`2025-11-25`) server the bridge runs the
//! `initialize` handshake and NATs its own session-id onto the upstream
//! `Mcp-Session-Id` header (ACT-SESSIONS §3.2). Against a **modern**
//! (`2026-07-28`) server there is no upstream session to NAT — SEP-2575
//! removed protocol-level sessions — so the ACT session carries only the
//! per-client config (url, credential key) it was opened with.
//!
//! # Where the credential comes from, and when
//!
//! The bearer token is **not** a session argument. It lives in the host's
//! credential store, is named by `credential_key` in the session args, and is
//! fetched with `act:credentials/store` — so it never passes through the
//! agent's context (ACT-AUTH §1.1). `credential_key` is optional: a bridge
//! pointed at an unauthenticated MCP server names none and nothing is
//! fetched.
//!
//! That moves the upstream handshake. `get-secret` requires a **live**
//! session, and the host marks a session live only *after* `open-session`
//! returns (ACT-AUTH §1.1.4), so the credential cannot be read at open — and
//! the dialect probe has to carry it, because an authenticated server answers
//! an unauthenticated `initialize` with a 401. Both therefore happen lazily,
//! on the first tool call, and are cached for the session's lifetime
//! ([`ensure_upstream`]).
//!
//! `open-session` consequently no longer reports a bad URL, a 401 or a
//! protocol mismatch: it validates its arguments and returns an id, and the
//! *first tool call* is where the upstream is reached. ACT-AUTH §1.1.4 states
//! the trade explicitly — "a component that would otherwise fail fast at open
//! time trades that for a first-call failure".

#![allow(clippy::all)]

mod creds;
mod mapping;
mod mcp_client;

wit_bindgen::generate!({
    path: "wit",
    world: "component-world",
    generate_all,
});

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use exports::act::sessions::session_provider as session_exports;
use exports::act::tools::tool_provider as tool_exports;
// In act:tools@0.2.0 the data model moved to a function-free `types`
// interface; `localized-string` lives in act:core. The `tool-provider`
// export module no longer re-exports these, so reference them directly.
use act::core::types::LocalizedString;
use act::tools::types::ToolDefinition;
use creds::Bearer;
use mcp_client::{Config, Dialect, McpError};

// ── Per-session state ──────────────────────────────────────────────────────

struct UpstreamSession {
    config: Config,
    /// Filled on the first tool call, never at open: the credential cannot be
    /// fetched until the session is live (ACT-AUTH §1.1.4), and the dialect
    /// probe has to carry it.
    upstream: Option<Upstream>,
    /// The refusal a poisoned session answers with.
    ///
    /// Set when the credential was refused — by the host (denied, absent,
    /// unreadable, expired) or by the upstream (401/403). None of those change
    /// within a session: the key is fixed at open and the host does not
    /// refresh a stored token. Without this the next tool call repeats the
    /// whole sequence *including `get-secret`*, which can put a consent prompt
    /// in front of a human on every single call.
    ///
    /// The original error is kept rather than a bool plus a generic sentence,
    /// so the second call says exactly what the first one did — including the
    /// command that fixes it.
    rejected: Option<McpError>,
}

/// What one successful first call bought, kept for the session's lifetime.
#[derive(Clone)]
struct Upstream {
    /// Wire dialect negotiated with this upstream, plus whatever per-dialect
    /// state it implies (the legacy `Mcp-Session-Id`, if the server issued one).
    dialect: Dialect,
    /// `None` when the session named no `credential_key` — an unauthenticated
    /// upstream, and no `Authorization` header is sent at all.
    auth: Option<Bearer>,
}

thread_local! {
    static SESSIONS: RefCell<HashMap<String, UpstreamSession>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

fn alloc_session_id() -> String {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        format!("mcp_{id}")
    })
}

/// Snapshot the per-session pieces needed to dispatch a request, connecting
/// to the upstream first if this is the session's first call.
///
/// Idempotent: a session that already has an [`Upstream`] touches neither the
/// credential store nor the network.
///
/// **Not de-duplicated across concurrent first calls.** Two calls that arrive
/// before either has connected both run the whole sequence, and the second
/// overwrites the first's cache with an equivalent one. The fix is a
/// per-session in-flight latch, which is more state for a race no MCP or HTTP
/// client in this workspace can currently produce — both are
/// request/response.
async fn ensure_upstream(session_id: &str) -> Result<(Config, Upstream), McpError> {
    let Some((config, cached, rejected)) = SESSIONS.with(|s| {
        s.borrow()
            .get(session_id)
            .map(|u| (u.config.clone(), u.upstream.clone(), u.rejected.clone()))
    }) else {
        return Err(unknown_session(session_id));
    };
    if let Some(upstream) = cached {
        return Ok((config, upstream));
    }
    // Before the store, not after: the whole point of the flag is that
    // `get-secret` is never reached a second time.
    if let Some(refusal) = rejected {
        return Err(refusal);
    }

    let connected = connect(session_id, &config).await;
    let upstream = match connected {
        Ok(upstream) => upstream,
        Err(e) => {
            note_refusal(session_id, &e);
            return Err(e);
        }
    };
    SESSIONS.with(|s| {
        if let Some(entry) = s.borrow_mut().get_mut(session_id) {
            entry.upstream = Some(upstream.clone());
        }
    });
    Ok((config, upstream))
}

/// Fetch the credential (when one is named) and negotiate the dialect.
async fn connect(session_id: &str, config: &Config) -> Result<Upstream, McpError> {
    let auth = match &config.credential_key {
        Some(key) => Some(fetch_bearer(session_id, config, key).await?),
        None => None,
    };
    let dialect = mcp_client::negotiate(config, auth.as_ref()).await?;
    Ok(Upstream { dialect, auth })
}

/// Poison the session when the failure is one that cannot change within it.
/// A transport failure or a timeout is left alone — those do change on their
/// own, and retrying them costs no consent prompt.
fn note_refusal(session_id: &str, e: &McpError) {
    if !e.poisons_the_session() {
        return;
    }
    SESSIONS.with(|s| {
        if let Some(entry) = s.borrow_mut().get_mut(session_id) {
            entry.rejected.get_or_insert_with(|| e.clone());
        }
    });
}

/// Ask the host credential store for this session's bearer token.
///
/// Depends on **field names only** — the two `creds` declares — because no
/// field name is well-known (`ACT-CONSTANTS.md` §8.2 registers types, not
/// names). `secret-request.kind` is left `None`: it names nothing under the
/// current model and MUST NOT filter retrieval (ACT-AUTH §1.1.6).
async fn fetch_bearer(session_id: &str, config: &Config, key: &str) -> Result<Bearer, McpError> {
    let want = act::credentials::types::SecretRequest {
        key: key.to_string(),
        kind: None,
        // Scheme and authority only — see `Config::resource`.
        resource: Some(config.resource()),
        scopes: vec![],
        hint: Some("Bearer token for the upstream MCP server".to_string()),
    };

    let raw = match act::credentials::store::get_secret(session_id.to_string(), want).await {
        Ok(raw) => raw,
        Err(e) => return Err(secret_error(e, key).await),
    };
    // Values cross as CBOR; `from_wit` decodes the field map. Its error names
    // the field and never its bytes.
    let secret = act_sdk::credentials::Secret::from_wit(raw.kind, raw.fields)
        .map_err(|e| McpError::internal(format!("credential field decode failed: {e}")))?;
    creds::bearer_from_secret(&secret, key, now_unix())
}

/// Unix seconds, or `0` when the host will not say — see
/// `creds::bearer_from_secret`, which treats `0` as "expiry unknowable"
/// rather than "everything is expired".
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Map a store refusal onto the error the agent sees.
///
/// `not-found` and `denied` collapse into one message on purpose: the host
/// decides `denied` **before** it consults the store (ACT-AUTH §1.1.7), so
/// distinguishing them here would invent a difference the host refuses to
/// disclose — and would turn the pair into a way to probe a profile for keys.
async fn secret_error(e: act::credentials::types::SecretError, key: &str) -> McpError {
    use act::credentials::types::SecretError;
    match e {
        SecretError::NotFound | SecretError::Denied => {
            // Best-effort: a policy that denies the store denies the listing
            // too, and then the message simply carries no inventory. Keys are
            // not secret — `list-secrets` exists to hand them to the agent.
            let known: Vec<String> = act::credentials::store::list_secrets(None)
                .await
                .map(|v| v.into_iter().map(|i| i.key).collect())
                .unwrap_or_default();
            creds::credential_missing(key, &known)
        }
        SecretError::InvalidSession => McpError {
            kind: act_types::constants::ERR_SESSION_NOT_FOUND.to_string(),
            message: "the credential store does not recognise this session; open a new one"
                .to_string(),
        },
        SecretError::Unavailable(msg) => {
            McpError::internal(format!("the credential store is unavailable: {msg}"))
        }
    }
}

fn unknown_session(session_id: &str) -> McpError {
    McpError {
        kind: act_types::constants::ERR_SESSION_NOT_FOUND.to_string(),
        message: format!("Unknown session-id: {session_id}"),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn extract_session_id(metadata: &[(String, Vec<u8>)]) -> Option<String> {
    metadata
        .iter()
        .find(|(k, _)| k == "std:session-id")
        .and_then(|(_, v)| {
            ciborium::from_reader::<serde_json::Value, _>(v.as_slice())
                .ok()
                .and_then(|val| match val {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                })
        })
}

fn invalid_args(msg: impl Into<String>) -> tool_exports::Error {
    tool_exports::Error {
        kind: act_types::constants::ERR_INVALID_ARGS.to_string(),
        message: LocalizedString::Plain(msg.into()),
        metadata: vec![],
    }
}

fn mcp_to_wit_error(e: &McpError) -> tool_exports::Error {
    tool_exports::Error {
        kind: e.kind.clone(),
        message: LocalizedString::Plain(e.message.clone()),
        metadata: vec![],
    }
}

// ── Component entry point ──────────────────────────────────────────────────

struct McpBridge;

export!(McpBridge);

// ── tool-provider ──────────────────────────────────────────────────────────

impl tool_exports::Guest for McpBridge {
    async fn list_tools(
        metadata: Vec<(String, Vec<u8>)>,
    ) -> Result<tool_exports::ListToolsResponse, tool_exports::Error> {
        let session_id = match extract_session_id(&metadata) {
            Some(id) => id,
            None => {
                return Ok(tool_exports::ListToolsResponse {
                    metadata: vec![],
                    tools: vec![],
                });
            }
        };

        // First call on this session: fetch the credential and negotiate.
        let (config, upstream) = ensure_upstream(&session_id)
            .await
            .map_err(|e| mcp_to_wit_error(&e))?;

        let result = mcp_client::mcp_request(
            &config,
            upstream.auth.as_ref(),
            &upstream.dialect,
            "tools/list",
            serde_json::json!({}),
        )
        .await
        .inspect_err(|e| note_refusal(&session_id, e))
        .map_err(|e| mcp_to_wit_error(&e))?;

        let list_result: rmcp::model::ListToolsResult =
            serde_json::from_value(result).map_err(|e| {
                mcp_to_wit_error(&McpError::internal(format!(
                    "Failed to parse tools/list response: {e}"
                )))
            })?;

        let tools: Vec<ToolDefinition> = list_result
            .tools
            .iter()
            .map(mapping::mcp_tool_to_act)
            .collect();

        Ok(tool_exports::ListToolsResponse {
            metadata: vec![],
            tools,
        })
    }

    async fn call_tool(
        name: String,
        arguments: Vec<u8>,
        metadata: Vec<(String, Vec<u8>)>,
    ) -> tool_exports::ToolResult {
        let session_id = match extract_session_id(&metadata) {
            Some(id) => id,
            None => {
                return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                    invalid_args("Missing required metadata key std:session-id"),
                )]);
            }
        };

        // First call on this session: fetch the credential and negotiate.
        let (config, upstream) = match ensure_upstream(&session_id).await {
            Ok(pair) => pair,
            Err(e) => {
                return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                    mcp_to_wit_error(&e),
                )]);
            }
        };

        // Decode arguments from CBOR to JSON.
        let args_json: serde_json::Value = if arguments.is_empty() {
            serde_json::json!({})
        } else {
            match act_types::cbor::cbor_to_json(&arguments) {
                Ok(v) => v,
                Err(e) => {
                    return tool_exports::ToolResult::Immediate(vec![
                        tool_exports::ToolEvent::Error(invalid_args(format!(
                            "Failed to decode arguments: {e}"
                        ))),
                    ]);
                }
            }
        };

        // `arguments` is sent even when empty: servers that declare a required
        // object schema reject a call that omits the field entirely.
        let params =
            rmcp::model::CallToolRequestParams::new(name).with_arguments(match args_json {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            });

        let result = match mcp_client::mcp_request(
            &config,
            upstream.auth.as_ref(),
            &upstream.dialect,
            "tools/call",
            serde_json::to_value(&params).unwrap_or_default(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                // A 401 mid-session poisons it too: the key is fixed at open,
                // so nothing this session can do will change the answer.
                note_refusal(&session_id, &e);
                return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                    mcp_to_wit_error(&e),
                )]);
            }
        };

        // SEP-2322 (MRTR) and SEP-2663 (Tasks) let `tools/call` answer with
        // something other than a finished result. Neither shape deserializes
        // into `CallToolResult`, so discriminate on `resultType` first and
        // fail loudly — a bare parse error here would read as a broken server.
        if let Some(result_type) = result.get("resultType").and_then(|v| v.as_str())
            && let Some(message) = unsupported_result_type(result_type)
        {
            return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                mcp_to_wit_error(&McpError::internal(message)),
            )]);
        }

        let call_result: rmcp::model::CallToolResult = match serde_json::from_value(result) {
            Ok(r) => r,
            Err(e) => {
                return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                    mcp_to_wit_error(&McpError::internal(format!(
                        "Failed to parse tools/call response: {e}"
                    ))),
                )]);
            }
        };

        tool_exports::ToolResult::Immediate(mapping::mcp_result_to_events(&call_result))
    }
}

/// Explain a `tools/call` result shape the bridge cannot forward, or `None`
/// when the shape is an ordinary completed result.
///
/// TODO(act-mrtr): a Multi Round-Trip Request (SEP-2322) asks the *client* to
/// fulfil sampling / elicitation / roots requests and retry the call with
/// `inputResponses` + the echoed `requestState`. ACT has no mid-call
/// "input required" channel for a component to hand that back to its caller,
/// so wiring it up needs a spec decision first (an ACT-SESSIONS or
/// ACT-SPEC-level interaction event). Until then the call fails explicitly
/// rather than silently returning an empty result.
fn unsupported_result_type(result_type: &str) -> Option<String> {
    match result_type {
        "input_required" => Some(
            "Upstream MCP server returned an MRTR input-required result (SEP-2322). \
             The bridge cannot fulfil server-initiated sampling/elicitation/roots \
             requests: ACT has no mid-call input channel yet, so this tool cannot \
             be called through mcp-bridge."
                .to_string(),
        ),
        "task" => Some(
            "Upstream MCP server materialized a task for this call (SEP-2663). \
             The bridge does not poll tasks/get, so the result cannot be retrieved."
                .to_string(),
        ),
        _ => None,
    }
}

// ── session-provider ───────────────────────────────────────────────────────

impl session_exports::Guest for McpBridge {
    async fn get_open_session_args_schema(
        _metadata: Vec<(String, Vec<u8>)>,
    ) -> Result<String, session_exports::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string(&schema).map_err(|e| session_exports::Error {
            kind: act_types::constants::ERR_INTERNAL.to_string(),
            message: LocalizedString::Plain(format!("Schema serialization failed: {e}")),
            metadata: vec![],
        })
    }

    async fn open_session(
        args: Vec<(String, Vec<u8>)>,
        _metadata: Vec<(String, Vec<u8>)>,
    ) -> Result<session_exports::Session, session_exports::Error> {
        let mut json_map = serde_json::Map::with_capacity(args.len());
        for (k, v) in &args {
            if let Ok(val) = ciborium::from_reader::<serde_json::Value, _>(v.as_slice()) {
                json_map.insert(k.clone(), val);
            }
        }
        let config: Config =
            serde_json::from_value(serde_json::Value::Object(json_map)).map_err(|e| {
                session_exports::Error {
                    kind: act_types::constants::ERR_INVALID_ARGS.to_string(),
                    message: LocalizedString::Plain(format!("Invalid open-session args: {e}")),
                    metadata: vec![],
                }
            })?;

        // Everything that can be decided without the network or the store:
        // the URL must be an address and must not carry userinfo, and
        // `credential_key` must be a lookup name rather than a sentence — the
        // host pastes it raw into the line a human reads when releasing a
        // credential.
        //
        // The upstream is deliberately NOT contacted here. Negotiation needs
        // the bearer token, and the token cannot be fetched until this call
        // has returned and the host has marked the session live (ACT-AUTH
        // §1.1.4). See `ensure_upstream`.
        config.validate().map_err(|e| session_exports::Error {
            kind: e.kind.clone(),
            message: LocalizedString::Plain(e.message.clone()),
            metadata: vec![],
        })?;

        let id = alloc_session_id();
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                UpstreamSession {
                    config,
                    upstream: None,
                    rejected: None,
                },
            );
        });

        Ok(session_exports::Session {
            id,
            metadata: vec![],
        })
    }

    fn close_session(session_id: String) {
        let session = SESSIONS.with(|s| s.borrow_mut().remove(&session_id));
        // Nothing to tear down for a session that never reached the upstream:
        // no dialect was negotiated, so no upstream state exists.
        if let Some(UpstreamSession {
            config,
            upstream: Some(upstream),
            ..
        }) = session
        {
            // Fire-and-forget: tell the upstream we're done (a no-op in the
            // modern dialect, which holds no per-client state). close-session
            // is sync per WIT, so we kick this off via wit_bindgen::spawn.
            wit_bindgen::spawn_local(async move {
                mcp_client::close_upstream(&config, upstream.auth.as_ref(), &upstream.dialect)
                    .await;
            });
        }
    }
}
