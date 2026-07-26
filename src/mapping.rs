// MCP <-> ACT type conversion utilities.
//
// The MCP side is `rmcp::model` — the reference Rust SDK's wire types, which
// model both the `2025-11-25` and `2026-07-28` revisions. Its content and
// tool types are `#[non_exhaustive]`, so every match here carries a wildcard
// arm that surfaces a future variant rather than dropping it.

// act:tools@0.2.0 moved the data model to the function-free `types`
// interface; `error`/`tool-event` are still re-exported via `tool-provider`,
// and `localized-string` lives in act:core.
use crate::act::core::types::LocalizedString;
use crate::act::tools::types::{ContentPart, ToolDefinition};
use crate::exports::act::tools::tool_provider::{Error as ToolError, ToolEvent};
use act_types::cbor::to_cbor;
use act_types::constants::{
    ERR_INTERNAL, META_DESTRUCTIVE, META_IDEMPOTENT, META_READ_ONLY, MIME_CBOR, MIME_OCTET_STREAM,
    MIME_TEXT,
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents, Tool};

/// MCP shapes that ACT has no first-class content type for are carried as
/// CBOR under a self-describing mime type, so a consumer can tell them apart
/// from a tool's own `structuredContent` (which uses plain `application/cbor`).
const MIME_RESOURCE_LINK: &str = "application/vnd.mcp.resource-link+cbor";
const MIME_CONTENT_BLOCK: &str = "application/vnd.mcp.content-block+cbor";

/// Vendor-prefixed part metadata carrying the URI of an embedded resource.
/// ACT has no well-known key for it, and dropping it would throw away the
/// only handle the caller has on the underlying resource.
const META_RESOURCE_URI: &str = "mcp:resource-uri";

/// Convert an MCP tool definition to an ACT `ToolDefinition`.
pub fn mcp_tool_to_act(tool: &Tool) -> ToolDefinition {
    // `description` is optional in MCP. Fall back to the display `title`
    // before giving up, so a title-only tool still reaches the agent with a
    // hint rather than an empty string.
    let description = LocalizedString::Plain(
        tool.description
            .as_deref()
            .or(tool.title.as_deref())
            .unwrap_or_default()
            .to_string(),
    );

    let parameters_schema = serde_json::to_string(tool.input_schema.as_ref())
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
        name: tool.name.to_string(),
        description,
        parameters_schema,
        metadata,
    }
}

/// Convert an MCP `tools/call` result to a list of ACT `ToolEvent`s.
pub fn mcp_result_to_events(result: &CallToolResult) -> Vec<ToolEvent> {
    if result.is_error == Some(true) {
        return vec![ToolEvent::Error(ToolError {
            kind: ERR_INTERNAL.to_string(),
            message: LocalizedString::Plain(error_message(result)),
            metadata: vec![],
        })];
    }

    let mut events = Vec::with_capacity(result.content.len() + 1);

    // `structuredContent` is the authoritative machine-readable form of a
    // result; MCP keeps `content` as the human-readable, backwards-compatible
    // mirror of it ("a tool that returns structured content SHOULD also
    // return the serialized JSON in a TextContent block"). Emit it first and
    // CBOR-encoded — ACT's native structured encoding — so a consumer reading
    // the leading part gets the richest representation, while text-only
    // consumers still find the mirror in the parts that follow.
    if let Some(structured) = &result.structured_content {
        events.push(ToolEvent::Content(ContentPart {
            data: to_cbor(structured),
            mime_type: Some(MIME_CBOR.to_string()),
            metadata: vec![],
        }));
    }

    for block in &result.content {
        match block {
            ContentBlock::Text(text) => events.push(ToolEvent::Content(ContentPart {
                data: text.text.as_bytes().to_vec(),
                mime_type: Some(MIME_TEXT.to_string()),
                metadata: vec![],
            })),
            ContentBlock::Image(image) => events.push(ToolEvent::Content(ContentPart {
                data: decode_base64(&image.data),
                mime_type: Some(image.mime_type.clone()),
                metadata: vec![],
            })),
            ContentBlock::Audio(audio) => events.push(ToolEvent::Content(ContentPart {
                data: decode_base64(&audio.data),
                mime_type: Some(audio.mime_type.clone()),
                metadata: vec![],
            })),
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    ..
                } => events.push(ToolEvent::Content(ContentPart {
                    data: text.as_bytes().to_vec(),
                    mime_type: Some(mime_type.clone().unwrap_or_else(|| MIME_TEXT.to_string())),
                    metadata: vec![(META_RESOURCE_URI.to_string(), to_cbor(uri))],
                })),
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => events.push(ToolEvent::Content(ContentPart {
                    data: decode_base64(blob),
                    mime_type: Some(
                        mime_type
                            .clone()
                            .unwrap_or_else(|| MIME_OCTET_STREAM.to_string()),
                    ),
                    metadata: vec![(META_RESOURCE_URI.to_string(), to_cbor(uri))],
                })),
                other => events.push(opaque_block(other, MIME_CONTENT_BLOCK)),
            },
            // A link carries no bytes — only a pointer plus the metadata that
            // describes it. Serialise the whole record so the uri, name,
            // description, mime type and size all survive.
            ContentBlock::ResourceLink(link) => events.push(opaque_block(link, MIME_RESOURCE_LINK)),
            // `ContentBlock` is #[non_exhaustive]: a variant added by a future
            // MCP revision is surfaced verbatim rather than silently dropped.
            other => events.push(opaque_block(other, MIME_CONTENT_BLOCK)),
        }
    }

    events
}

/// Best message we can produce for an `isError: true` result.
fn error_message(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        return text;
    }
    // Servers that answer only with `structuredContent` would otherwise
    // produce an empty error message.
    if let Some(structured) = &result.structured_content {
        return structured.to_string();
    }
    "MCP tool reported an error with no message".to_string()
}

fn opaque_block<T: serde::Serialize>(value: &T, mime_type: &str) -> ToolEvent {
    ToolEvent::Content(ContentPart {
        data: to_cbor(value),
        mime_type: Some(mime_type.to_string()),
        metadata: vec![],
    })
}

/// MCP carries binary payloads as base64 strings. A payload that does not
/// decode is passed through verbatim rather than dropped.
fn decode_base64(data: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap_or_else(|_| data.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use act_types::cbor::cbor_to_json;
    use base64::Engine;
    use rmcp::model::{EmbeddedResource, Resource, ToolAnnotations};
    use serde_json::json;

    fn schema(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn content_part(event: &ToolEvent) -> &ContentPart {
        match event {
            ToolEvent::Content(part) => part,
            _ => panic!("expected content event"),
        }
    }

    fn success(content: Vec<ContentBlock>) -> CallToolResult {
        CallToolResult::success(content)
    }

    // ── tool definitions ───────────────────────────────────────────────

    #[test]
    fn basic_tool_mapping() {
        let tool = Tool::new(
            "get_weather",
            "Get current weather",
            schema(json!({
                "type": "object",
                "properties": { "city": { "type": "string" } }
            })),
        );
        let def = mcp_tool_to_act(&tool);
        assert_eq!(def.name, "get_weather");
        assert!(
            matches!(def.description, LocalizedString::Plain(ref s) if s == "Get current weather")
        );
        assert!(def.parameters_schema.contains("\"type\":\"object\""));
        assert!(def.metadata.is_empty());
    }

    #[test]
    fn tool_with_annotations() {
        let tool = Tool::new(
            "read_file",
            "Read a file",
            schema(json!({"type": "object"})),
        )
        .with_annotations(ToolAnnotations::from_raw(
            None,
            Some(true),
            Some(false),
            Some(true),
            None,
        ));
        let def = mcp_tool_to_act(&tool);
        assert_eq!(def.metadata.len(), 2);
        assert!(def.metadata.iter().any(|(k, _)| k == META_READ_ONLY));
        assert!(def.metadata.iter().any(|(k, _)| k == META_IDEMPOTENT));
    }

    #[test]
    fn default_description_when_missing() {
        let tool = Tool::new_with_raw("simple", None, schema(json!({"type": "object"})));
        let def = mcp_tool_to_act(&tool);
        assert!(matches!(def.description, LocalizedString::Plain(ref s) if s.is_empty()));
    }

    #[test]
    fn title_stands_in_for_a_missing_description() {
        let tool = Tool::new_with_raw("simple", None, schema(json!({"type": "object"})))
            .with_title("Simple Tool");
        let def = mcp_tool_to_act(&tool);
        assert!(matches!(def.description, LocalizedString::Plain(ref s) if s == "Simple Tool"));
    }

    // ── content blocks ─────────────────────────────────────────────────

    #[test]
    fn text_content_to_events() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::text("Hello, world!")]));
        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.data, b"Hello, world!");
        assert_eq!(part.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn image_content_to_events() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::image(
            b64(b"\x89PNG"),
            "image/png",
        )]));
        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.data, b"\x89PNG");
        assert_eq!(part.mime_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn audio_content_to_events() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::audio(
            b64(b"RIFF\x00\x00"),
            "audio/wav",
        )]));
        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.data, b"RIFF\x00\x00");
        assert_eq!(part.mime_type.as_deref(), Some("audio/wav"));
    }

    #[test]
    fn resource_text_to_events() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::Resource(
            EmbeddedResource::new(ResourceContents::text(
                "file contents",
                "file:///tmp/test.txt",
            )),
        )]));
        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.data, b"file contents");
        assert_eq!(part.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn resource_blob_to_events() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::Resource(
            EmbeddedResource::new(
                ResourceContents::blob(b64(b"\x00\x01\x02"), "file:///tmp/data.bin")
                    .with_mime_type("application/octet-stream"),
            ),
        )]));
        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.data, b"\x00\x01\x02");
        assert_eq!(part.mime_type.as_deref(), Some("application/octet-stream"));
    }

    #[test]
    fn embedded_resource_keeps_its_uri_in_part_metadata() {
        let events = mcp_result_to_events(&success(vec![ContentBlock::Resource(
            EmbeddedResource::new(ResourceContents::text("x", "file:///tmp/test.txt")),
        )]));
        let part = content_part(&events[0]);
        let (key, value) = &part.metadata[0];
        assert_eq!(key, META_RESOURCE_URI);
        assert_eq!(cbor_to_json(value).unwrap(), json!("file:///tmp/test.txt"));
    }

    #[test]
    fn resource_link_survives_as_a_tagged_cbor_part() {
        let link = Resource::new("file:///report.pdf", "report.pdf")
            .with_description("Quarterly report")
            .with_mime_type("application/pdf")
            .with_size(4096);
        let events = mcp_result_to_events(&success(vec![ContentBlock::ResourceLink(link)]));

        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.mime_type.as_deref(), Some(MIME_RESOURCE_LINK));

        let decoded = cbor_to_json(&part.data).unwrap();
        assert_eq!(decoded["uri"], json!("file:///report.pdf"));
        assert_eq!(decoded["name"], json!("report.pdf"));
        assert_eq!(decoded["description"], json!("Quarterly report"));
        assert_eq!(decoded["mimeType"], json!("application/pdf"));
        assert_eq!(decoded["size"], json!(4096));
    }

    #[test]
    fn empty_content() {
        assert!(mcp_result_to_events(&success(vec![])).is_empty());
    }

    // ── structured content ─────────────────────────────────────────────

    #[test]
    fn structured_content_leads_the_event_list() {
        let mut result = success(vec![ContentBlock::text(r#"{"temp":22.5}"#)]);
        result.structured_content = Some(json!({ "temp": 22.5 }));
        let events = mcp_result_to_events(&result);

        assert_eq!(events.len(), 2);
        let structured = content_part(&events[0]);
        assert_eq!(structured.mime_type.as_deref(), Some(MIME_CBOR));
        assert_eq!(
            cbor_to_json(&structured.data).unwrap(),
            json!({ "temp": 22.5 })
        );
        // The text mirror is preserved for consumers that only read text.
        let mirror = content_part(&events[1]);
        assert_eq!(mirror.mime_type.as_deref(), Some(MIME_TEXT));
        assert_eq!(mirror.data, br#"{"temp":22.5}"#);
    }

    #[test]
    fn structured_content_without_a_text_mirror() {
        let mut result = success(vec![]);
        result.structured_content = Some(json!([1, 2, 3]));
        let events = mcp_result_to_events(&result);

        assert_eq!(events.len(), 1);
        let part = content_part(&events[0]);
        assert_eq!(part.mime_type.as_deref(), Some(MIME_CBOR));
        assert_eq!(cbor_to_json(&part.data).unwrap(), json!([1, 2, 3]));
    }

    // ── errors ─────────────────────────────────────────────────────────

    #[test]
    fn error_result_to_events() {
        let events = mcp_result_to_events(&CallToolResult::error(vec![ContentBlock::text(
            "Something went wrong",
        )]));
        assert_eq!(events.len(), 1);
        match &events[0] {
            ToolEvent::Error(e) => {
                assert_eq!(e.kind, ERR_INTERNAL);
                assert!(
                    matches!(&e.message, LocalizedString::Plain(s) if s == "Something went wrong")
                );
            }
            _ => panic!("expected error event"),
        }
    }

    #[test]
    fn error_result_falls_back_to_structured_content() {
        let result = CallToolResult::structured_error(json!({ "code": "E_NOPE" }));
        let events = mcp_result_to_events(&result);
        match &events[0] {
            // `structured_error` also fills a text mirror, so that wins.
            ToolEvent::Error(e) => {
                assert!(matches!(&e.message, LocalizedString::Plain(s) if s.contains("E_NOPE")));
            }
            _ => panic!("expected error event"),
        }

        let mut bare = CallToolResult::error(vec![]);
        bare.structured_content = Some(json!({ "code": "E_NOPE" }));
        match &mcp_result_to_events(&bare)[0] {
            ToolEvent::Error(e) => {
                assert!(matches!(&e.message, LocalizedString::Plain(s) if s.contains("E_NOPE")));
            }
            _ => panic!("expected error event"),
        }
    }

    #[test]
    fn error_result_with_no_message_still_says_something() {
        match &mcp_result_to_events(&CallToolResult::error(vec![]))[0] {
            ToolEvent::Error(e) => {
                assert!(matches!(&e.message, LocalizedString::Plain(s) if !s.is_empty()));
            }
            _ => panic!("expected error event"),
        }
    }
}
