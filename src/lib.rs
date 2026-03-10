mod mcp_client;
mod mapping;

wit_bindgen::generate!({
    path: "wit",
    world: "act-world",
});

struct McpBridge;

export!(McpBridge);

impl exports::act::core::tool_provider::Guest for McpBridge {
    fn get_info() -> act::core::types::ComponentInfo {
        act::core::types::ComponentInfo {
            name: "mcp-bridge".to_string(),
            version: "0.1.0".to_string(),
            default_language: "en".to_string(),
            description: act::core::types::LocalizedString::Plain(
                "Proxies a remote MCP server's tools as ACT tools".to_string(),
            ),
            capabilities: vec![],
            metadata: vec![],
        }
    }

    fn get_config_schema() -> Option<String> {
        let schema = schemars::schema_for!(mcp_client::Config);
        Some(serde_json::to_string(&schema).unwrap())
    }

    async fn list_tools(
        _config: Option<Vec<u8>>,
    ) -> Result<act::core::types::ListToolsResponse, act::core::types::ToolError> {
        Ok(act::core::types::ListToolsResponse {
            metadata: vec![],
            tools: vec![],
        })
    }

    async fn call_tool(
        _config: Option<Vec<u8>>,
        _call: act::core::types::ToolCall,
    ) -> act::core::types::CallResponse {
        let (_writer, reader) = wit_stream::new::<act::core::types::StreamEvent>();
        act::core::types::CallResponse {
            metadata: vec![],
            body: reader,
        }
    }
}
