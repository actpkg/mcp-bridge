//! mcp-bridge — proxy a remote MCP server's tools as local ACT tools.
//!
//! Each `open-session` does an MCP `initialize` handshake against the
//! upstream and stashes the resulting `Mcp-Session-Id` (if any) for the
//! lifetime of the bridge-issued session-id. Subsequent capability
//! calls reference the bridge id via `std:session-id`; the bridge maps
//! it back to the upstream session header (NAT-style — ACT-SESSIONS §3.2).

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
use mcp_client::{Config, McpError};

// ── Per-session state ──────────────────────────────────────────────────────

struct UpstreamSession {
    config: Config,
    /// Mcp-Session-Id from the upstream initialize handshake. None when
    /// the server doesn't issue session ids (stateless MCP servers).
    upstream_session_id: Option<String>,
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
fn snapshot_session(session_id: &str) -> Option<(Config, Option<String>)> {
    SESSIONS.with(|s| {
        s.borrow()
            .get(session_id)
            .map(|u| (u.config.clone(), u.upstream_session_id.clone()))
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
        message: tool_exports::LocalizedString::Plain(msg.into()),
        metadata: vec![],
    }
}

fn session_not_found(session_id: &str) -> tool_exports::Error {
    tool_exports::Error {
        kind: act_types::constants::ERR_SESSION_NOT_FOUND.to_string(),
        message: tool_exports::LocalizedString::Plain(format!("Unknown session-id: {session_id}")),
        metadata: vec![],
    }
}

fn mcp_to_wit_error(e: &McpError) -> tool_exports::Error {
    tool_exports::Error {
        kind: e.kind.clone(),
        message: tool_exports::LocalizedString::Plain(e.message.clone()),
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

        let (config, mcp_sid) = match snapshot_session(&session_id) {
            Some(s) => s,
            None => return Err(session_not_found(&session_id)),
        };

        let result = mcp_client::mcp_request(
            &config,
            mcp_sid.as_deref(),
            "tools/list",
            serde_json::json!({}),
        )
        .await
        .map_err(|e| mcp_to_wit_error(&e))?;

        let list_result: act_types::mcp::ListToolsResult =
            serde_json::from_value(result).map_err(|e| {
                mcp_to_wit_error(&McpError::internal(format!(
                    "Failed to parse tools/list response: {e}"
                )))
            })?;

        let tools: Vec<tool_exports::ToolDefinition> = list_result
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

        let (config, mcp_sid) = match snapshot_session(&session_id) {
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

        let params = act_types::mcp::CallToolParams {
            name,
            arguments: Some(args_json),
        };

        let result = match mcp_client::mcp_request(
            &config,
            mcp_sid.as_deref(),
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

        let call_result: act_types::mcp::CallToolResult = match serde_json::from_value(result) {
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

// ── session-provider ───────────────────────────────────────────────────────

impl session_exports::Guest for McpBridge {
    async fn get_open_session_args_schema(
        _metadata: Vec<(String, Vec<u8>)>,
    ) -> Result<String, session_exports::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string(&schema).map_err(|e| session_exports::Error {
            kind: act_types::constants::ERR_INTERNAL.to_string(),
            message: tool_exports::LocalizedString::Plain(format!(
                "Schema serialization failed: {e}"
            )),
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
                    message: tool_exports::LocalizedString::Plain(format!(
                        "Invalid open-session args: {e}"
                    )),
                    metadata: vec![],
                }
            })?;

        // Run the upstream initialize handshake — failure surfaces as a
        // proper session-open error so the agent sees auth / connect
        // problems before issuing a session-id (per ACT-SESSIONS §2.2).
        let upstream_session_id =
            mcp_client::initialize(&config)
                .await
                .map_err(|e| session_exports::Error {
                    kind: e.kind.clone(),
                    message: tool_exports::LocalizedString::Plain(e.message.clone()),
                    metadata: vec![],
                })?;

        let id = alloc_session_id();
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                UpstreamSession {
                    config,
                    upstream_session_id,
                },
            );
        });

        Ok(session_exports::Session {
            id,
            metadata: vec![],
        })
    }

    fn close_session(session_id: String) {
        let upstream = SESSIONS.with(|s| {
            s.borrow_mut()
                .remove(&session_id)
                .and_then(|u| u.upstream_session_id.map(|sid| (u.config, sid)))
        });
        if let Some((config, sid)) = upstream {
            // Fire-and-forget: tell the upstream we're done. close-session
            // is sync per WIT, so we kick this off via wit_bindgen::spawn.
            wit_bindgen::spawn(async move {
                mcp_client::close_upstream(&config, &sid).await;
            });
        }
    }
}
