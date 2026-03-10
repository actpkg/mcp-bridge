// MCP <-> ACT type conversion utilities.

use crate::act::core::types::{
    ContentPart, LocalizedString, StreamEvent, ToolDefinition, ToolError,
};
use act_types::cbor::to_cbor;
use act_types::constants::{
    ERR_INTERNAL, META_DESTRUCTIVE, META_IDEMPOTENT, META_READ_ONLY, MIME_TEXT,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// Convert an MCP tool JSON object to an ACT `ToolDefinition`.
///
/// Returns `None` if the required `name` field is missing.
pub fn mcp_tool_to_act(tool: &serde_json::Value) -> Option<ToolDefinition> {
    let name = tool.get("name")?.as_str()?.to_string();

    let description = LocalizedString::Plain(
        tool.get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    );

    let parameters_schema = tool
        .get("inputSchema")
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| r#"{"type":"object"}"#.to_string()))
        .unwrap_or_else(|| r#"{"type":"object"}"#.to_string());

    let mut metadata: Vec<(String, Vec<u8>)> = Vec::new();
    let cbor_true = to_cbor(&true);

    if let Some(annotations) = tool.get("annotations") {
        if annotations.get("readOnlyHint") == Some(&serde_json::Value::Bool(true)) {
            metadata.push((META_READ_ONLY.to_string(), cbor_true.clone()));
        }
        if annotations.get("idempotentHint") == Some(&serde_json::Value::Bool(true)) {
            metadata.push((META_IDEMPOTENT.to_string(), cbor_true.clone()));
        }
        if annotations.get("destructiveHint") == Some(&serde_json::Value::Bool(true)) {
            metadata.push((META_DESTRUCTIVE.to_string(), cbor_true.clone()));
        }
    }

    Some(ToolDefinition {
        name,
        description,
        parameters_schema,
        metadata,
    })
}

/// Extract concatenated text from all `type: "text"` content items in an MCP result.
fn extract_text_content(result: &serde_json::Value) -> String {
    let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    content
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert an MCP `tools/call` result to a list of ACT `StreamEvent`s.
pub fn mcp_result_to_events(result: &serde_json::Value) -> Vec<StreamEvent> {
    // If the result signals an error, return a single error event.
    if result.get("isError") == Some(&serde_json::Value::Bool(true)) {
        let message = extract_text_content(result);
        return vec![StreamEvent::Error(ToolError {
            kind: ERR_INTERNAL.to_string(),
            message: LocalizedString::Plain(message),
            metadata: vec![],
        })];
    }

    let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
        return vec![];
    };

    let mut events = Vec::with_capacity(content.len());

    for item in content {
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match item_type {
            "text" => {
                let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                events.push(StreamEvent::Content(ContentPart {
                    data: text.as_bytes().to_vec(),
                    mime_type: Some(MIME_TEXT.to_string()),
                    metadata: vec![],
                }));
            }
            "image" => {
                let data_str = item.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let data = BASE64.decode(data_str).unwrap_or_default();
                let mime_type = item
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png")
                    .to_string();
                events.push(StreamEvent::Content(ContentPart {
                    data,
                    mime_type: Some(mime_type),
                    metadata: vec![],
                }));
            }
            "resource" => {
                if let Some(resource) = item.get("resource") {
                    if let Some(text) = resource.get("text").and_then(|t| t.as_str()) {
                        let mime_type = resource
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or(MIME_TEXT)
                            .to_string();
                        events.push(StreamEvent::Content(ContentPart {
                            data: text.as_bytes().to_vec(),
                            mime_type: Some(mime_type),
                            metadata: vec![],
                        }));
                    } else if let Some(blob) = resource.get("blob").and_then(|b| b.as_str()) {
                        let data = BASE64.decode(blob).unwrap_or_default();
                        let mime_type = resource
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        events.push(StreamEvent::Content(ContentPart {
                            data,
                            mime_type: Some(mime_type),
                            metadata: vec![],
                        }));
                    }
                    // If resource has neither text nor blob, skip it.
                }
            }
            _ => {
                // Unknown content type — skip.
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn basic_tool_mapping() {
        let tool = json!({
            "name": "get_weather",
            "description": "Get current weather",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "city": { "type": "string" }
                }
            }
        });
        let def = mcp_tool_to_act(&tool).unwrap();
        assert_eq!(def.name, "get_weather");
        assert!(matches!(def.description, LocalizedString::Plain(ref s) if s == "Get current weather"));
        assert!(def.parameters_schema.contains("\"type\":\"object\""));
        assert!(def.metadata.is_empty());
    }

    #[test]
    fn tool_with_annotations() {
        let tool = json!({
            "name": "read_file",
            "description": "Read a file",
            "annotations": {
                "readOnlyHint": true,
                "idempotentHint": true,
                "destructiveHint": false
            }
        });
        let def = mcp_tool_to_act(&tool).unwrap();
        assert_eq!(def.metadata.len(), 2);
        assert!(def.metadata.iter().any(|(k, _)| k == META_READ_ONLY));
        assert!(def.metadata.iter().any(|(k, _)| k == META_IDEMPOTENT));
    }

    #[test]
    fn missing_name_returns_none() {
        let tool = json!({ "description": "No name" });
        assert!(mcp_tool_to_act(&tool).is_none());
    }

    #[test]
    fn default_schema_when_missing() {
        let tool = json!({ "name": "simple" });
        let def = mcp_tool_to_act(&tool).unwrap();
        assert_eq!(def.parameters_schema, r#"{"type":"object"}"#);
    }

    #[test]
    fn text_content_to_events() {
        let result = json!({
            "content": [
                { "type": "text", "text": "Hello, world!" }
            ]
        });
        let events = mcp_result_to_events(&result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Content(cp) => {
                assert_eq!(cp.data, b"Hello, world!");
                assert_eq!(cp.mime_type.as_deref(), Some("text/plain"));
            }
            _ => panic!("expected content event"),
        }
    }

    #[test]
    fn error_result_to_events() {
        let result = json!({
            "isError": true,
            "content": [
                { "type": "text", "text": "Something went wrong" }
            ]
        });
        let events = mcp_result_to_events(&result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error(e) => {
                assert_eq!(e.kind, ERR_INTERNAL);
                assert!(matches!(&e.message, LocalizedString::Plain(s) if s == "Something went wrong"));
            }
            _ => panic!("expected error event"),
        }
    }

    #[test]
    fn image_content_to_events() {
        let data = BASE64.encode(b"\x89PNG");
        let result = json!({
            "content": [
                { "type": "image", "data": data, "mimeType": "image/png" }
            ]
        });
        let events = mcp_result_to_events(&result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Content(cp) => {
                assert_eq!(cp.data, b"\x89PNG");
                assert_eq!(cp.mime_type.as_deref(), Some("image/png"));
            }
            _ => panic!("expected content event"),
        }
    }

    #[test]
    fn resource_text_to_events() {
        let result = json!({
            "content": [
                {
                    "type": "resource",
                    "resource": {
                        "uri": "file:///tmp/test.txt",
                        "text": "file contents",
                        "mimeType": "text/plain"
                    }
                }
            ]
        });
        let events = mcp_result_to_events(&result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Content(cp) => {
                assert_eq!(cp.data, b"file contents");
                assert_eq!(cp.mime_type.as_deref(), Some("text/plain"));
            }
            _ => panic!("expected content event"),
        }
    }

    #[test]
    fn resource_blob_to_events() {
        let blob = BASE64.encode(b"\x00\x01\x02");
        let result = json!({
            "content": [
                {
                    "type": "resource",
                    "resource": {
                        "uri": "file:///tmp/data.bin",
                        "blob": blob,
                        "mimeType": "application/octet-stream"
                    }
                }
            ]
        });
        let events = mcp_result_to_events(&result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Content(cp) => {
                assert_eq!(cp.data, b"\x00\x01\x02");
            }
            _ => panic!("expected content event"),
        }
    }

    #[test]
    fn empty_content_array() {
        let result = json!({ "content": [] });
        assert!(mcp_result_to_events(&result).is_empty());
    }

    #[test]
    fn missing_content_field() {
        let result = json!({});
        assert!(mcp_result_to_events(&result).is_empty());
    }
}
