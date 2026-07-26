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
//! per-client config (url, auth token) that ACT-AUTH puts in session args.

#![allow(clippy::all)]

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
use mcp_client::{Config, Dialect, McpError};

// ── Per-session state ──────────────────────────────────────────────────────

struct UpstreamSession {
    config: Config,
    /// Wire dialect negotiated with this upstream, plus whatever per-dialect
    /// state it implies (the legacy `Mcp-Session-Id`, if the server issued one).
    dialect: Dialect,
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

/// Snapshot the per-session pieces needed to dispatch a request.
fn snapshot_session(session_id: &str) -> Option<(Config, Dialect)> {
    SESSIONS.with(|s| {
        s.borrow()
            .get(session_id)
            .map(|u| (u.config.clone(), u.dialect.clone()))
    })
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

fn session_not_found(session_id: &str) -> tool_exports::Error {
    tool_exports::Error {
        kind: act_types::constants::ERR_SESSION_NOT_FOUND.to_string(),
        message: LocalizedString::Plain(format!("Unknown session-id: {session_id}")),
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

        let (config, dialect) = match snapshot_session(&session_id) {
            Some(s) => s,
            None => return Err(session_not_found(&session_id)),
        };

        let result =
            mcp_client::mcp_request(&config, &dialect, "tools/list", serde_json::json!({}))
                .await
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

        let (config, dialect) = match snapshot_session(&session_id) {
            Some(s) => s,
            None => {
                return tool_exports::ToolResult::Immediate(vec![tool_exports::ToolEvent::Error(
                    session_not_found(&session_id),
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
            &dialect,
            "tools/call",
            serde_json::to_value(&params).unwrap_or_default(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
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

        // Negotiate the dialect up front — failure surfaces as a proper
        // session-open error so the agent sees auth / connect / protocol
        // problems before a session-id is issued (per ACT-SESSIONS §2.2).
        let dialect = mcp_client::negotiate(&config)
            .await
            .map_err(|e| session_exports::Error {
                kind: e.kind.clone(),
                message: LocalizedString::Plain(e.message.clone()),
                metadata: vec![],
            })?;

        let id = alloc_session_id();
        SESSIONS.with(|s| {
            s.borrow_mut()
                .insert(id.clone(), UpstreamSession { config, dialect });
        });

        Ok(session_exports::Session {
            id,
            metadata: vec![],
        })
    }

    fn close_session(session_id: String) {
        let upstream = SESSIONS.with(|s| s.borrow_mut().remove(&session_id));
        if let Some(upstream) = upstream {
            // Fire-and-forget: tell the upstream we're done (a no-op in the
            // modern dialect, which holds no per-client state). close-session
            // is sync per WIT, so we kick this off via wit_bindgen::spawn.
            wit_bindgen::spawn_local(async move {
                mcp_client::close_upstream(&upstream.config, &upstream.dialect).await;
            });
        }
    }
}
