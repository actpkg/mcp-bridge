# Call echo tool on hello-world MCP server
POST {{baseurl}}/tools/echo
{
  "arguments": {"message": "World"},
  "metadata": {"url": "{{mcpurl}}"}
}
HTTP 200
[Asserts]
jsonpath "$.content[0].data" contains "World"