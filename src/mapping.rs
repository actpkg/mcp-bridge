// MCP <-> ACT type conversion utilities.

use crate::act::core::types::{
    ContentPart, LocalizedString, StreamEvent, ToolDefinition, ToolError,
};
use act_types::cbor::to_cbor;
use act_types::constants::{
    ERR_INTERNAL, META_DESTRUCTIVE, META_IDEMPOTENT, META_READ_ONLY, MIME_TEXT,
};
use act_types::mcp::{CallToolResult, ContentItem};

/// Convert an MCP tool definition to an ACT `ToolDefinition`.
pub fn mcp_tool_to_act(tool: &act_types::mcp::ToolDefinition) -> ToolDefinition {
    let description = LocalizedString::Plain(
        tool.description.clone().unwrap_or_default(),
    );

    let parameters_schema = serde_json::to_string(&tool.input_schema)
        .unwrap_or_else(|_| r#"{"type":"object"}"#.to_string());

    let mut metadata: Vec<(String, Vec<u8>)> = Vec::new();
    let cbor_true = to_cbor(&true);

    if let Some(ref ann) = tool.annotations {
        if ann.read_only_hint == Some(true) {
            metadata.push((META_READ_ONLY.to_string(), cbor_true.clone()));
        }
        if ann.idempotent_hint == Some(true) {
            metadata.push((META_IDEMPOTENT.to_string(), cbor_true.clone()));
        }
        if ann.destructive_hint == Some(true) {
            metadata.push((META_DESTRUCTIVE.to_string(), cbor_true.clone()));
        }
    }

    ToolDefinition {
        name: tool.name.clone(),
        description,
        parameters_schema,
        metadata,
    }
}

/// Convert an MCP `tools/call` result to a list of ACT `StreamEvent`s.
pub fn mcp_result_to_events(result: &CallToolResult) -> Vec<StreamEvent> {
    if result.is_error == Some(true) {
        let message = result
            .content
            .iter()
            .filter_map(|item| match item {
                ContentItem::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return vec![StreamEvent::Error(ToolError {
            kind: ERR_INTERNAL.to_string(),
            message: LocalizedString::Plain(message),
            metadata: vec![],
        })];
    }

    let mut events = Vec::with_capacity(result.content.len());

    for item in &result.content {
        match item {
            ContentItem::Text(t) => {
                events.push(StreamEvent::Content(ContentPart {
                    data: t.text.as_bytes().to_vec(),
                    mime_type: Some(MIME_TEXT.to_string()),
                    metadata: vec![],
                }));
            }
            ContentItem::Image(img) => {
                events.push(StreamEvent::Content(ContentPart {
                    data: img.data.clone(),
                    mime_type: Some(img.mime_type.clone()),
                    metadata: vec![],
                }));
            }
            ContentItem::Resource(res) => {
                let resource = &res.resource;
                if let Some(ref text) = resource.text {
                    let mime_type = resource
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| MIME_TEXT.to_string());
                    events.push(StreamEvent::Content(ContentPart {
                        data: text.as_bytes().to_vec(),
                        mime_type: Some(mime_type),
                        metadata: vec![],
                    }));
                } else if let Some(ref blob) = resource.blob {
                    let mime_type = resource
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    events.push(StreamEvent::Content(ContentPart {
                        data: blob.clone(),
                        mime_type: Some(mime_type),
                        metadata: vec![],
                    }));
                }
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use act_types::mcp::{
        EmbeddedResource, ImageContent, ResourceContent, TextContent, ToolAnnotations,
    };
    use serde_json::json;

    #[test]
    fn basic_tool_mapping() {
        let tool = act_types::mcp::ToolDefinition {
            name: "get_weather".to_string(),
            description: Some("Get current weather".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string" }
                }
            }),
            annotations: None,
        };
        let def = mcp_tool_to_act(&tool);
        assert_eq!(def.name, "get_weather");
        assert!(matches!(def.description, LocalizedString::Plain(ref s) if s == "Get current weather"));
        assert!(def.parameters_schema.contains("\"type\":\"object\""));
        assert!(def.metadata.is_empty());
    }

    #[test]
    fn tool_with_annotations() {
        let tool = act_types::mcp::ToolDefinition {
            name: "read_file".to_string(),
            description: Some("Read a file".to_string()),
            input_schema: json!({"type": "object"}),
            annotations: Some(ToolAnnotations {
                read_only_hint: Some(true),
                idempotent_hint: Some(true),
                destructive_hint: Some(false),
                open_world_hint: None,
            }),
        };
        let def = mcp_tool_to_act(&tool);
        assert_eq!(def.metadata.len(), 2);
        assert!(def.metadata.iter().any(|(k, _)| k == META_READ_ONLY));
        assert!(def.metadata.iter().any(|(k, _)| k == META_IDEMPOTENT));
    }

    #[test]
    fn default_description_when_missing() {
        let tool = act_types::mcp::ToolDefinition {
            name: "simple".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        };
        let def = mcp_tool_to_act(&tool);
        assert!(matches!(def.description, LocalizedString::Plain(ref s) if s.is_empty()));
    }

    #[test]
    fn text_content_to_events() {
        let result = CallToolResult {
            content: vec![ContentItem::Text(TextContent {
                text: "Hello, world!".to_string(),
            })],
            is_error: None,
        };
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
        let result = CallToolResult {
            content: vec![ContentItem::Text(TextContent {
                text: "Something went wrong".to_string(),
            })],
            is_error: Some(true),
        };
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
        let result = CallToolResult {
            content: vec![ContentItem::Image(ImageContent {
                data: b"\x89PNG".to_vec(),
                mime_type: "image/png".to_string(),
            })],
            is_error: None,
        };
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
        let result = CallToolResult {
            content: vec![ContentItem::Resource(ResourceContent {
                resource: EmbeddedResource {
                    uri: "file:///tmp/test.txt".to_string(),
                    text: Some("file contents".to_string()),
                    blob: None,
                    mime_type: Some("text/plain".to_string()),
                },
            })],
            is_error: None,
        };
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
        let result = CallToolResult {
            content: vec![ContentItem::Resource(ResourceContent {
                resource: EmbeddedResource {
                    uri: "file:///tmp/data.bin".to_string(),
                    text: None,
                    blob: Some(b"\x00\x01\x02".to_vec()),
                    mime_type: Some("application/octet-stream".to_string()),
                },
            })],
            is_error: None,
        };
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
    fn empty_content() {
        let result = CallToolResult {
            content: vec![],
            is_error: None,
        };
        assert!(mcp_result_to_events(&result).is_empty());
    }
}
