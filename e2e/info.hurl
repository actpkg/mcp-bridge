GET {{host}}/info
HTTP 200
[Asserts]
jsonpath "$.name" == "mcp-bridge"
jsonpath "$.version" isString
