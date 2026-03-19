# List tools from upstream MCP server via bridge
POST {{baseurl}}/tools
{
  "metadata": {"url": "{{mcp_url}}"}
}
HTTP 200
[Asserts]
jsonpath "$.tools" count >= 1
jsonpath "$.tools[*].name" contains "validate_url"
