# Without std:session-id, list-tools returns an empty list and
# call-tool errors with std:invalid-args.

POST {{baseurl}}/tools
{}
HTTP 200
[Asserts]
jsonpath "$.tools" count == 0

POST {{baseurl}}/tools/anything
{"arguments": {}}
HTTP *
[Asserts]
jsonpath "$.error.kind" == "std:invalid-args"
jsonpath "$.error.message" contains "std:session-id"
