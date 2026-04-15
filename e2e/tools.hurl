# List tools from hello-world MCP server
POST {{baseurl}}/tools
{
  "metadata": {"url": "{{mcpurl}}"}
}
HTTP 200
[Asserts]
jsonpath "$.tools" count >= 1
jsonpath "$.tools[*].name" contains "echo"
