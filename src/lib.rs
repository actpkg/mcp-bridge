mod mapping;
mod mcp_client;

wit_bindgen::generate!({
    path: "wit",
    world: "component-world",
    generate_all,
});

use act::core::types::*;
use act_types::cbor;

// WASM custom sections for component metadata.
// SAFETY: link_section places data in named WASM custom sections; no executable code.
#[unsafe(link_section = "act:component")]
#[used]
static _ACT_COMPONENT: [u8; include_bytes!(concat!(env!("OUT_DIR"), "/act_component.cbor")).len()] =
    *include_bytes!(concat!(env!("OUT_DIR"), "/act_component.cbor"));

#[unsafe(link_section = "version")]
#[used]
static _VERSION: [u8; 5] = *b"0.1.0";

#[unsafe(link_section = "description")]
#[used]
static _DESCRIPTION: [u8; 48] = *b"Proxies a remote MCP server's tools as ACT tools";

struct McpBridge;

export!(McpBridge);

/// Helper: create a response stream from events.
fn respond(events: Vec<StreamEvent>) -> wit_bindgen::rt::async_support::StreamReader<StreamEvent> {
    let (mut writer, reader) = wit_stream::new::<StreamEvent>();
    wit_bindgen::spawn(async move {
        writer.write_all(events).await;
    });
    reader
}

/// Helper: create a ToolError from McpError.
fn to_tool_error(e: &mcp_client::McpError) -> ToolError {
    ToolError {
        kind: e.kind.clone(),
        message: LocalizedString::Plain(e.message.clone()),
        metadata: vec![],
    }
}

impl exports::act::core::tool_provider::Guest for McpBridge {
    async fn get_metadata_schema(_metadata: Vec<(String, Vec<u8>)>) -> Option<String> {
        let schema = schemars::schema_for!(mcp_client::Config);
        Some(serde_json::to_string(&schema).unwrap())
    }

    async fn list_tools(metadata: Vec<(String, Vec<u8>)>) -> Result<ListToolsResponse, ToolError> {
        let config =
            mcp_client::parse_config_from_metadata(&metadata).map_err(|e| to_tool_error(&e))?;

        let result = mcp_client::mcp_request(&config, "tools/list", serde_json::json!({}))
            .await
            .map_err(|e| to_tool_error(&e))?;

        let list_result: act_types::mcp::ListToolsResult =
            serde_json::from_value(result).map_err(|e| ToolError {
                kind: act_types::constants::ERR_INTERNAL.to_string(),
                message: LocalizedString::Plain(format!(
                    "Failed to parse tools/list response: {e}"
                )),
                metadata: vec![],
            })?;

        let tools: Vec<ToolDefinition> = list_result
            .tools
            .iter()
            .map(mapping::mcp_tool_to_act)
            .collect();

        Ok(ListToolsResponse {
            metadata: vec![],
            tools,
        })
    }

    async fn call_tool(
        call: ToolCall,
    ) -> wit_bindgen::rt::async_support::StreamReader<StreamEvent> {
        let events = match call_tool_inner(call).await {
            Ok(events) => events,
            Err(e) => vec![StreamEvent::Error(to_tool_error(&e))],
        };

        respond(events)
    }
}

async fn call_tool_inner(call: ToolCall) -> Result<Vec<StreamEvent>, mcp_client::McpError> {
    let config = mcp_client::parse_config_from_metadata(&call.metadata)?;

    // Decode arguments from dCBOR to JSON
    let arguments: serde_json::Value = if call.arguments.is_empty() {
        serde_json::json!({})
    } else {
        cbor::cbor_to_json(&call.arguments).map_err(|e| {
            mcp_client::McpError::invalid_args(format!("Failed to decode arguments: {e}"))
        })?
    };

    let params = act_types::mcp::CallToolParams {
        name: call.name,
        arguments: Some(arguments),
    };

    let result = mcp_client::mcp_request(
        &config,
        "tools/call",
        serde_json::to_value(&params).unwrap(),
    )
    .await?;

    let call_result: act_types::mcp::CallToolResult =
        serde_json::from_value(result).map_err(|e| {
            mcp_client::McpError::internal(format!("Failed to parse tools/call response: {e}"))
        })?;

    Ok(mapping::mcp_result_to_events(&call_result))
}
