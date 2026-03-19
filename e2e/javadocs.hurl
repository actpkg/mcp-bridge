# javadocs.dev — Java documentation MCP server

# List tools
POST {{baseurl}}/tools
{
  "metadata": {"url": "https://www.javadocs.dev/mcp"}
}
HTTP 200
[Asserts]
jsonpath "$.tools" count >= 1
jsonpath "$.tools[*].name" contains "get_latest_version"

# Get latest version of a well-known library
POST {{baseurl}}/tools/get_latest_version
{
  "arguments": {"groupId": "com.google.guava", "artifactId": "guava"},
  "metadata": {"url": "https://www.javadocs.dev/mcp"}
}
HTTP 200
[Asserts]
jsonpath "$.content[0].data" isString
