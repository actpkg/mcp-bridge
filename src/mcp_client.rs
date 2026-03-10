use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
pub struct Config {
    /// MCP server URL (e.g. http://localhost:3000/mcp)
    pub url: String,
    /// Optional Bearer token for authentication
    pub auth_token: Option<String>,
}
