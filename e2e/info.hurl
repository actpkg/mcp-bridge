GET {{baseurl}}/info
HTTP 200
[Asserts]
jsonpath "$['std:name']" == "mcp-bridge"
jsonpath "$['std:version']" isString
