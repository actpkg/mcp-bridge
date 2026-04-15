# Call nonexistent tool — should return error
POST {{baseurl}}/tools/nonexistent_tool_xyz
{
  "arguments": {},
  "metadata": {"url": "{{mcpurl}}"}
}
HTTP 422
[Asserts]
jsonpath "$.error" exists
