GET {{baseurl}}/info
HTTP 200
[Asserts]
jsonpath "$.std.name" "mcp_bridge"
jsonpath "$.std.version" isString
