//! Optional GCF encoding of `query_graphql` MCP tool text.
//!
//! Gated by `[gql].gcf` (off by default). When on, the GraphQL JSON envelope
//! is encoded as GCF generic profile (<https://gcformat.com/>) so agents see
//! fewer tokens. Encode failure falls back to JSON so the call still returns.

use std::borrow::Cow;

use rmcp::model::Tool;
use serde_json::Value;

/// Trailing sentence of the static `query_graphql` tool description.
pub const JSON_RETURNS: &str =
    "Returns the standard GraphQL response `{ \"data\": ..., \"errors\": ... }`.";

/// Replacement for [`JSON_RETURNS`] when `[gql].gcf` is on.
pub const GCF_RETURNS: &str = "Returns the GraphQL envelope (`data` / `errors`) as GCF generic \
     profile (https://gcformat.com/), not JSON. Pipe-tabular GCF; models read it natively — \
     do not request JSON.";

/// Extra line appended to server instructions when GCF output is on.
pub const GCF_INSTRUCTIONS: &str =
    "\n\nOutput: GCF generic profile (https://gcformat.com/), not JSON. \
     Same GraphQL `data`/`errors` envelope, pipe-tabular instead of braces.";

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
}
