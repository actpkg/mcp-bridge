# zip1.io — URL shortener MCP server

# List tools
POST {{baseurl}}/tools
{
  "metadata": {"url": "https://zip1.io/mcp"}
}
HTTP 200
[Asserts]
jsonpath "$.tools" count >= 1
jsonpath "$.tools[*].name" contains "validate_url"

# Call validate_url
POST {{baseurl}}/tools/validate_url
{
  "arguments": {"url": "https://github.com"},
  "metadata": {"url": "https://zip1.io/mcp"}
}
HTTP 200
[Asserts]
jsonpath "$.content[0].data" contains "github.com"

# Call non-existent tool
POST {{baseurl}}/tools/nonexistent_tool
{
  "arguments": {},
  "metadata": {"url": "https://zip1.io/mcp"}
}
HTTP *
[Asserts]
jsonpath "$.content[0].data" contains "Unknown tool"
