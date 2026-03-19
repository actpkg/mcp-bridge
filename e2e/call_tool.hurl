# Call validate_url tool via bridge
POST {{baseurl}}/tools/validate_url
{
  "arguments": {"url": "https://github.com"},
  "metadata": {"url": "{{mcp_url}}"}
}
HTTP 200
[Asserts]
jsonpath "$.content[0].data" contains "github.com"
