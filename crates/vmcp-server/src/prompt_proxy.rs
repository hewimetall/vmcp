//! Upstream prompt aggregation helpers.
//!
//! Used by [`crate::proxy::ProxyServer`] (`/mcp-proxy` — `prompts/list` +
//! `prompts/get` with `{server}__{prompt}` names) and by GraphQL
//! `getPrompt` on `/mcp`. Argument values are coerced to JSON strings
//! (MCP prompts/get contract). Each get response is prepended with a
//! GraphQL routing table for that upstream's tools.

use rmcp::model::*;
use serde_json::{Map, Value};

use vmcp_upstream::{ResolvedPrompt, UpstreamPool};

use crate::graphql_inject::{
    build_graphql_tool_injection, prepend_injection, select_tools_for_injection,
};

pub(crate) const NAME_SEP: &str = "__";

/// Coerce every argument value to a JSON string (MCP prompts/get contract).
/// Shared by `/mcp-proxy` prompts/get and GraphQL `getPrompt`.
pub(crate) fn normalize_prompt_args(
    args: Option<Map<String, Value>>,
) -> Option<Map<String, Value>> {
    args.map(|m| {
        m.into_iter()
            .map(|(k, v)| {
                let s = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, Value::String(s))
            })
            .collect()
    })
}

/// Build the prefixed MCP prompt catalogue from the pool snapshot.
pub(crate) fn catalogue_from_pool(pool: &UpstreamPool) -> Vec<Prompt> {
    let mut prompts: Vec<Prompt> = Vec::new();
    for (server, list) in pool.all_prompts() {
        for p in list {
            prompts.push(prompt_from_resolved(&server, &p));
        }
    }
    prompts
}

fn prompt_from_resolved(server: &str, p: &ResolvedPrompt) -> Prompt {
    let prefixed = format!("{server}{NAME_SEP}{}", p.name);
    let args = if p.arguments.is_empty() {
        None
    } else {
        Some(
            p.arguments
                .iter()
                .map(|a| {
                    let mut pa = PromptArgument::new(a.name.clone());
                    if let Some(d) = &a.description {
                        pa = pa.with_description(d.clone());
                    }
                    pa = pa.with_required(a.required);
                    pa
                })
                .collect(),
        )
    };
    let desc = match &p.description {
        Some(d) => format!("[{server}] {d}"),
        None => format!("[{server}] {}", p.name),
    };
    Prompt::new(prefixed, Some(desc), args)
}

/// Inject GraphQL routing into an upstream `GetPromptResult`.
///
/// Tool rows are narrowed to names mentioned in the prompt body when possible;
/// otherwise the full upstream catalogue is injected.
pub(crate) fn inject_into_result(
    server: &str,
    tools: &[vmcp_upstream::ResolvedTool],
    mut upstream: GetPromptResult,
) -> GetPromptResult {
    let body_preview = preview_messages_text(&upstream.messages);
    let selected = select_tools_for_injection(tools, &body_preview);
    let injection = build_graphql_tool_injection(server, &selected);
    if upstream.messages.is_empty() {
        upstream
            .messages
            .push(PromptMessage::new_text(PromptMessageRole::User, injection));
    } else {
        match &mut upstream.messages[0].content {
            PromptMessageContent::Text { text } => {
                *text = prepend_injection(&injection, text);
            }
            _ => {
                upstream.messages.insert(
                    0,
                    PromptMessage::new_text(PromptMessageRole::User, injection),
                );
            }
        }
    }
    upstream
}

fn preview_messages_text(messages: &[PromptMessage]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        if let PromptMessageContent::Text { text } = &m.content {
            parts.push(text.as_str());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmcp_notify::Bus;
    use vmcp_registry::TaskSupportHint;
    use vmcp_upstream::{ResolvedPromptArg, ResolvedTool};

    #[test]
    fn catalogue_prefixes_and_preserves_arguments() {
        let bus = Bus::new(64);
        let pool = UpstreamPool::empty_for_test(bus);
        pool.insert_synthetic_prompts_for_test(
            "demo",
            Some("Demo upstream".into()),
            vec![ResolvedTool {
                server: "demo".into(),
                name: "echo".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
                task_support: TaskSupportHint::Forbidden,
            }],
            vec![ResolvedPrompt {
                server: "demo".into(),
                name: "greet".into(),
                description: Some("Say hi".into()),
                arguments: vec![ResolvedPromptArg {
                    name: "who".into(),
                    description: Some("Name".into()),
                    required: true,
                }],
            }],
        );
        let listed = catalogue_from_pool(&pool);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "demo__greet");
        let args = listed[0].arguments.as_ref().unwrap();
        assert_eq!(args[0].name, "who");
        assert_eq!(args[0].required, Some(true));
        assert!(listed[0]
            .description
            .as_deref()
            .unwrap_or("")
            .contains("[demo]"));
    }

    #[test]
    fn inject_prepends_to_first_text_message() {
        let tools = vec![ResolvedTool {
            server: "demo".into(),
            name: "echo_tool".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        let upstream = GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "ORIGINAL BODY uses echo_tool",
        )]);
        let out = inject_into_result("demo", &tools, upstream);
        match &out.messages[0].content {
            PromptMessageContent::Text { text } => {
                assert!(text.contains("Query.demo.echoTool"));
                assert!(text.contains("ORIGINAL BODY uses echo_tool"));
                assert!(text.starts_with("## vmcp GraphQL routing"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn normalize_coerces_non_strings() {
        let mut m = Map::new();
        m.insert("limit".into(), Value::from(10));
        m.insert("ok".into(), Value::Bool(true));
        m.insert("q".into(), Value::String("hi".into()));
        let out = normalize_prompt_args(Some(m)).unwrap();
        assert_eq!(out.get("limit"), Some(&Value::String("10".into())));
        assert_eq!(out.get("ok"), Some(&Value::String("true".into())));
        assert_eq!(out.get("q"), Some(&Value::String("hi".into())));
    }
}
