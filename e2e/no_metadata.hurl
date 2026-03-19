# Call without metadata — should fail (url required)
POST {{baseurl}}/tools
{}
HTTP *
[Asserts]
jsonpath "$.error" exists
