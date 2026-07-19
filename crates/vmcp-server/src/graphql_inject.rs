//! Prepend GraphQL routing for upstream tools into prompt bodies.
//!
//! Upstream prompts talk about raw MCP tool names. Through vmcp the agent
//! must call `query_graphql` with `Query|Mutation.<serverCamel>.<toolCamel>`.
//! This module builds that mapping from [`ResolvedTool`] entries.
//!
//! When the prompt body mentions specific tool names, only those tools are
//! listed (token saver). If none match, the full upstream catalogue is used.

use vmcp_graphql::{camel_case, pascal_case};
use vmcp_upstream::ResolvedTool;

/// Prefer tools whose names appear in `body`; fall back to the full list.
pub fn select_tools_for_injection<'a>(
    tools: &'a [ResolvedTool],
    body: &str,
) -> Vec<&'a ResolvedTool> {
    if tools.is_empty() || body.is_empty() {
        return tools.iter().collect();
    }
    let lower = body.to_lowercase();
    let matched: Vec<&ResolvedTool> = tools
        .iter()
        .filter(|t| {
            let snake = t.name.to_lowercase();
            let camel = camel_case(&t.name).to_lowercase();
            // Also match common MCP-prefixed forms: mcp__server__tool, server__tool
            lower.contains(&snake) || lower.contains(&camel)
        })
        .collect();
    if matched.is_empty() {
        tools.iter().collect()
    } else {
        matched
    }
}

/// Build the markdown injection block for one upstream's tool catalogue.
pub fn build_graphql_tool_injection(server: &str, tools: &[&ResolvedTool]) -> String {
    let server_camel = camel_case(server);
    let mut lines = Vec::new();
    lines.push("## vmcp GraphQL routing (do not call raw MCP tools)".to_string());
    lines.push(String::new());
    lines.push(format!(
        "Call only `query_graphql`. Upstream `{server}` maps to GraphQL namespace `{server_camel}`:"
    ));
    lines.push(String::new());
    lines.push("| Upstream tool | GraphQL path | Kind |".to_string());
    lines.push("|---|---|---|".to_string());

    let mut sorted = tools.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for t in &sorted {
        let tool_camel = camel_case(&t.name);
        let (root, kind) = if t.read_only {
            ("Query", "read → parallel")
        } else {
            ("Mutation", "write → sequential")
        };
        let path = format!("{root}.{server_camel}.{tool_camel}(...)");
        lines.push(format!("| `{}` | `{path}` | {kind} |", t.name));
    }

    if sorted.is_empty() {
        lines.push("| _(none)_ | — | — |".to_string());
    }

    let example_tool = sorted
        .iter()
        .copied()
        .find(|t| t.read_only)
        .or_else(|| sorted.first().copied());

    lines.push(String::new());
    if let Some(ex) = example_tool {
        let tool_camel = camel_case(&ex.name);
        let op = if ex.read_only { "" } else { "mutation " };
        lines.push("Example:".to_string());
        lines.push(String::new());
        lines.push("```graphql".to_string());
        lines.push(format!("{op}{{"));
        lines.push(format!("  result: {server_camel} {{"));
        lines.push(format!(
            "    {tool_camel}(/* args */) {{ json text isError }}"
        ));
        lines.push("  }".to_string());
        lines.push("}".to_string());
        lines.push("```".to_string());
    }

    lines.push(String::new());
    lines.push(format!(
        "Introspect args via `__type(name: \"{}{}\")`. One document, aliased fields; \
         reads aggregate in parallel, mutations run sequentially.",
        pascal_case(server),
        if example_tool.map(|t| t.read_only).unwrap_or(true) {
            "Read"
        } else {
            "Write"
        }
    ));
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());

    lines.join("\n")
}

/// Prepend injection to an existing text body.
pub fn prepend_injection(injection: &str, body: &str) -> String {
    if body.is_empty() {
        injection.to_string()
    } else {
        format!("{injection}{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmcp_registry::TaskSupportHint;

    fn tool(server: &str, name: &str, read_only: bool) -> ResolvedTool {
        ResolvedTool {
            server: server.into(),
            name: name.into(),
            description: Some(format!("desc {name}")),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
            task_support: TaskSupportHint::Forbidden,
        }
    }

    #[test]
    fn injection_lists_query_and_mutation_paths() {
        let tools = [
            tool("tavily", "tavily_search", true),
            tool("tavily", "tavily_crawl", false),
        ];
        let refs: Vec<&ResolvedTool> = tools.iter().collect();
        let text = build_graphql_tool_injection("tavily", &refs);
        assert!(text.contains("Query.tavily.tavilySearch"));
        assert!(text.contains("Mutation.tavily.tavilyCrawl"));
        assert!(text.contains("query_graphql"));
        assert!(text.contains("do not call raw MCP tools"));
    }

    #[test]
    fn select_narrows_to_mentioned_tools() {
        let tools = vec![
            tool("tavily", "tavily_search", true),
            tool("tavily", "tavily_crawl", false),
            tool("tavily", "tavily_extract", true),
        ];
        let body = "First call tavily_search, then tavilyExtract if needed.";
        let selected = select_tools_for_injection(&tools, body);
        let names: Vec<&str> = selected.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tavily_search"));
        assert!(names.contains(&"tavily_extract"));
        assert!(!names.contains(&"tavily_crawl"));
    }

    #[test]
    fn select_falls_back_to_all_when_no_mention() {
        let tools = vec![
            tool("tavily", "tavily_search", true),
            tool("tavily", "tavily_crawl", false),
        ];
        let selected = select_tools_for_injection(&tools, "do the research workflow");
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn prepend_keeps_body() {
        let out = prepend_injection("HEAD\n", "BODY");
        assert_eq!(out, "HEAD\nBODY");
    }
}
