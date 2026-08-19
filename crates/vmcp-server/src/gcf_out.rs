//! Optional GCF encoding of MCP tool text.
//!
//! Two independent runtime flags:
//! * `[gql].gcf` — GraphQL `/mcp` `query_graphql` envelope
//! * `[proxy].gcf` — `/mcp-proxy` upstream tool results
//!
//! Both default off. Encode failure falls back to JSON so the call still returns.

use std::borrow::Cow;

use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde_json::Value;

/// Trailing sentence of the static `query_graphql` tool description.
pub const JSON_RETURNS: &str =
    "Returns the standard GraphQL response `{ \"data\": ..., \"errors\": ... }`.";

/// Replacement for [`JSON_RETURNS`] when `[gql].gcf` is on.
pub const GCF_RETURNS: &str = "Returns the GraphQL envelope (`data` / `errors`) as GCF generic \
     profile (https://gcformat.com/), not JSON. Pipe-tabular GCF; models read it natively — \
     do not request JSON.";

/// Extra line appended to GraphQL server instructions when `[gql].gcf` is on.
pub const GCF_INSTRUCTIONS: &str =
    "\n\nOutput: GCF generic profile (https://gcformat.com/), not JSON. \
     Same GraphQL `data`/`errors` envelope, pipe-tabular instead of braces.";

/// Extra line appended to `/mcp-proxy` instructions when `[proxy].gcf` is on.
pub const GCF_PROXY_INSTRUCTIONS: &str =
    "\n\nOutput: GCF generic profile (https://gcformat.com/), not JSON. \
     Structured / JSON tool results are encoded; plain-text errors stay as-is.";

/// Suffix appended to proxied tool descriptions when `[proxy].gcf` is on.
pub const GCF_PROXY_TOOL_NOTE: &str =
    "\n\nOutput: GCF generic profile (https://gcformat.com/), not JSON.";

/// Encode the GraphQL JSON envelope for the MCP tool text body.
///
/// When `gcf` is false this is `Value::to_string`. When true, GCF generic
/// profile; if the encoder rejects the value (e.g. integer outside i64),
/// JSON is returned so the agent still gets a payload.
pub fn render_tool_text(body: &Value, gcf: bool) -> String {
    if !gcf {
        return body.to_string();
    }
    match gcf::encode_generic(body) {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %e, "GCF encode failed; falling back to JSON");
            body.to_string()
        }
    }
}

/// Rewrite the advertised `query_graphql` description so the agent knows
/// the wire format is GCF. Other tools are left untouched. Idempotent.
pub fn annotate_query_graphql_tool(tool: &mut Tool) {
    if tool.name.as_ref() != "query_graphql" {
        return;
    }
    let Some(desc) = tool.description.as_deref() else {
        tool.description = Some(Cow::Borrowed(GCF_RETURNS));
        return;
    };
    if desc.contains("GCF generic profile") {
        return;
    }
    let rewritten = if desc.contains(JSON_RETURNS) {
        desc.replace(JSON_RETURNS, GCF_RETURNS)
    } else {
        format!("{desc}\n\n{GCF_RETURNS}")
    };
    tool.description = Some(Cow::Owned(rewritten));
}

/// Encode an upstream `CallToolResult` for `/mcp-proxy`.
///
/// When `gcf` is false the result is unchanged. When true:
/// * `structuredContent` (if present) or JSON text is encoded as GCF
/// * non-text blocks (image/audio) are kept
/// * plain non-JSON text (errors, freeform) is left untouched
pub fn encode_call_tool_result(mut result: CallToolResult, gcf: bool) -> CallToolResult {
    if !gcf {
        return result;
    }
    let payload = if let Some(sc) = result.structured_content.clone() {
        sc
    } else {
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => v,
            Err(_) => return result,
        }
    };
    let encoded = render_tool_text(&payload, true);
    let mut content = vec![ContentBlock::text(encoded)];
    content.extend(
        result
            .content
            .iter()
            .filter(|c| !matches!(c, ContentBlock::Text(_)))
            .cloned(),
    );
    result.content = content;
    result.structured_content = None;
    result
}

/// Append the GCF note to a proxied tool description. Idempotent.
pub fn annotate_proxy_tool_description(description: &mut Option<String>) {
    let Some(desc) = description.as_mut() else {
        *description = Some(GCF_PROXY_TOOL_NOTE.trim().to_string());
        return;
    };
    if desc.contains("GCF generic profile") {
        return;
    }
    desc.push_str(GCF_PROXY_TOOL_NOTE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Number};
    use std::sync::Arc;

    fn sample_envelope() -> Value {
        json!({
            "data": {
                "time": {
                    "getCurrentTime": {
                        "isError": false,
                        "json": {"timezone": "UTC", "unix": 1_700_000_000}
                    }
                }
            }
        })
    }

    #[test]
    fn json_path_is_compact_json() {
        let body = sample_envelope();
        let out = render_tool_text(&body, false);
        assert_eq!(out, body.to_string());
        assert!(out.starts_with('{'), "json: {out}");
        assert!(!out.starts_with("GCF"));
    }

    #[test]
    fn gcf_path_is_generic_profile_and_roundtrips() {
        let body = sample_envelope();
        let out = render_tool_text(&body, true);
        assert!(out.starts_with("GCF "), "expected GCF header, got: {out:?}");
        assert!(out.contains("profile=generic"), "header: {out}");
        let decoded = gcf::decode_generic(&out).expect("decode GCF");
        assert_eq!(decoded, body);
    }

    #[test]
    fn gcf_encode_failure_falls_back_to_json() {
        // SPEC 2.3.2: integers outside i64 are rejected rather than approximated.
        let body = Value::Number(Number::from(u64::MAX));
        let out = render_tool_text(&body, true);
        assert_eq!(out, body.to_string());
        assert!(!out.starts_with("GCF "));
    }

    fn dummy_tool(name: &str, desc: &str) -> Tool {
        Tool::new(
            name.to_string(),
            desc.to_string(),
            Arc::new(Default::default()),
        )
    }

    #[test]
    fn annotate_rewrites_query_graphql_returns_line() {
        let mut tool = dummy_tool("query_graphql", &format!("intro. {JSON_RETURNS}"));
        annotate_query_graphql_tool(&mut tool);
        let desc = tool.description.as_deref().unwrap().to_string();
        assert!(desc.contains("GCF generic profile"));
        assert!(!desc.contains(JSON_RETURNS));
        // Idempotent.
        annotate_query_graphql_tool(&mut tool);
        assert_eq!(tool.description.as_deref(), Some(desc.as_str()));
    }

    #[test]
    fn annotate_appends_when_returns_line_missing() {
        let mut tool = dummy_tool("query_graphql", "just a blurb");
        annotate_query_graphql_tool(&mut tool);
        let desc = tool.description.as_deref().unwrap();
        assert!(desc.contains("just a blurb"));
        assert!(desc.contains("GCF generic profile"));
    }

    #[test]
    fn annotate_fills_empty_description() {
        let mut tool = Tool::new_with_raw("query_graphql", None, Arc::new(Default::default()));
        annotate_query_graphql_tool(&mut tool);
        assert_eq!(tool.description.as_deref(), Some(GCF_RETURNS));
    }

    #[test]
    fn annotate_ignores_other_tools() {
        let mut tool = dummy_tool("run_task", JSON_RETURNS);
        annotate_query_graphql_tool(&mut tool);
        assert_eq!(tool.description.as_deref(), Some(JSON_RETURNS));
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn proxy_encode_off_is_passthrough() {
        let original = CallToolResult::success(vec![ContentBlock::text(
            json!({"timezone":"UTC"}).to_string(),
        )]);
        let out = encode_call_tool_result(original.clone(), false);
        assert_eq!(out, original);
    }

    #[test]
    fn proxy_encode_json_text() {
        let body = json!({"timezone":"UTC","unix":1_700_000_000});
        let result = CallToolResult::success(vec![ContentBlock::text(body.to_string())]);
        let out = encode_call_tool_result(result, true);
        let text = text_of(&out);
        assert!(text.starts_with("GCF "), "{text}");
        assert!(out.structured_content.is_none());
        assert_eq!(gcf::decode_generic(&text).unwrap(), body);
    }

    #[test]
    fn proxy_encode_prefers_structured_content() {
        let structured = json!({"rows":[{"id":1},{"id":2}]});
        let result = CallToolResult::structured(structured.clone());
        let out = encode_call_tool_result(result, true);
        let text = text_of(&out);
        assert!(text.starts_with("GCF "), "{text}");
        assert!(out.structured_content.is_none());
        assert_eq!(gcf::decode_generic(&text).unwrap(), structured);
    }

    #[test]
    fn proxy_encode_leaves_plain_text() {
        let result = CallToolResult::error(vec![ContentBlock::text("forbidden: no scope")]);
        let out = encode_call_tool_result(result.clone(), true);
        assert_eq!(out, result);
    }

    #[test]
    fn proxy_encode_keeps_non_text_blocks() {
        let body = json!({"ok": true});
        let mut result = CallToolResult::success(vec![ContentBlock::text(body.to_string())]);
        result
            .content
            .push(ContentBlock::image("AAAA", "image/png"));
        let out = encode_call_tool_result(result, true);
        assert!(matches!(out.content[0], ContentBlock::Text(_)));
        assert!(text_of(&out).starts_with("GCF "));
        assert!(matches!(out.content[1], ContentBlock::Image(_)));
    }

    #[test]
    fn annotate_proxy_description_appends_and_is_idempotent() {
        let mut desc = Some("[time] get current time".into());
        annotate_proxy_tool_description(&mut desc);
        let once = desc.clone().unwrap();
        assert!(once.contains("GCF generic profile"));
        annotate_proxy_tool_description(&mut desc);
        assert_eq!(desc.as_deref(), Some(once.as_str()));
        let mut empty = None;
        annotate_proxy_tool_description(&mut empty);
        assert!(empty.unwrap().contains("GCF generic profile"));
    }
}
