# Call non-existent tool — upstream returns error in content
POST {{baseurl}}/tools/nonexistent_tool
{
  "arguments": {},
  "metadata": {"url": "{{mcp_url}}"}
}
HTTP *
[Asserts]
jsonpath "$.content[0].data" contains "Unknown tool"
