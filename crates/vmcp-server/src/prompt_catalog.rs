//! GraphQL skill/prompt discovery handlers.
//!
//! Wires local YAML skills (+ optional upstream prompts when
//! `[proxy]` is enabled) into [`vmcp_graphql::PromptSourceHandlers`]
//! so agents can `{ prompts { … } }` / `{ getPrompt(name) { text } }` mid-flight.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{PromptMessageContent, PromptMessageRole};
use serde_json::{Map, Value};
use vmcp_graphql::{PromptArgMeta, PromptContent, PromptMeta, PromptSourceHandlers};
use vmcp_upstream::UpstreamPool;

use crate::prompt_proxy::{inject_into_result, normalize_prompt_args};
use crate::skills::render_skill;
use crate::SkillsHandle;

const NAME_SEP: &str = "__";

/// Build GraphQL prompt discovery handlers.
///
/// * Local YAML skills are always listed.
/// * Upstream prompts (`{server}__{name}`) are included only when
///   `include_upstream` is true (tied to `[proxy].enabled`).
pub fn prompt_source_handlers(
    skills: SkillsHandle,
    pool: Arc<UpstreamPool>,
    include_upstream: bool,
) -> PromptSourceHandlers {
    let skills_list = skills.clone();
    let pool_list = pool.clone();
    let list = Arc::new(move || list_all(&skills_list, &pool_list, include_upstream));

    let skills_get = skills.clone();
    let pool_get = pool.clone();
    let get = Arc::new(move |name: String, args: Option<Map<String, Value>>| -> _ {
        let skills = skills_get.clone();
        let pool = pool_get.clone();
        Box::pin(async move { get_one(&skills, &pool, &name, args, include_upstream).await })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<PromptContent, String>> + Send>,
            >
    });

    PromptSourceHandlers { list, get }
}

fn list_all(skills: &SkillsHandle, pool: &UpstreamPool, include_upstream: bool) -> Vec<PromptMeta> {
    let mut out = Vec::new();
    for s in skills.load().iter() {
        out.push(PromptMeta {
            name: s.name.clone(),
            description: s.description.clone(),
            source: "local".into(),
            server: None,
            arguments: s
                .arguments
                .iter()
                .map(|a| PromptArgMeta {
                    name: a.name.clone(),
                    description: a.description.clone(),
                    required: a.required,
                })
                .collect(),
        });
    }
    if include_upstream {
        for (server, prompts) in pool.all_prompts() {
            for p in prompts {
                out.push(PromptMeta {
                    name: format!("{server}{NAME_SEP}{}", p.name),
                    description: p
                        .description
                        .unwrap_or_else(|| format!("[{server}] {}", p.name)),
                    source: "upstream".into(),
                    server: Some(server.clone()),
                    arguments: p
                        .arguments
                        .into_iter()
                        .map(|a| PromptArgMeta {
                            name: a.name,
                            description: a.description,
                            required: a.required,
                        })
                        .collect(),
                });
            }
        }
    }
    out
}

async fn get_one(
    skills: &SkillsHandle,
    pool: &UpstreamPool,
    name: &str,
    args: Option<Map<String, Value>>,
    include_upstream: bool,
) -> Result<PromptContent, String> {
    if let Some((server, prompt)) = name.split_once(NAME_SEP) {
        if !include_upstream {
            return Err(format!("upstream prompt `{name}` requires [proxy] enabled"));
        }
        return get_upstream(pool, server, prompt, args).await;
    }
    get_local(skills, name, args)
}

fn get_local(
    skills: &SkillsHandle,
    name: &str,
    args: Option<Map<String, Value>>,
) -> Result<PromptContent, String> {
    let skills_snap = skills.load();
    let skill = skills_snap
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .ok_or_else(|| format!("unknown prompt: {name}"))?;
    let map = args_to_string_map(args);
    let rendered = render_skill(&skill, &map).map_err(|e| format!("render skill `{name}`: {e}"))?;
    Ok(PromptContent {
        description: Some(skill.description),
        text: rendered,
    })
}

async fn get_upstream(
    pool: &UpstreamPool,
    server: &str,
    prompt: &str,
    args: Option<Map<String, Value>>,
) -> Result<PromptContent, String> {
    // MCP prompts/get arguments are string maps per the spec.
    let args = normalize_prompt_args(args);
    let upstream = pool
        .get_prompt(server, prompt, args)
        .await
        .map_err(|e| format!("upstream `{server}` prompt `{prompt}`: {e}"))?;

    let tools = pool.resolved(server).unwrap_or_default();
    // Narrows injection to tools mentioned in the body when possible.
    let injected = inject_into_result(server, &tools, upstream);
    let text = messages_to_text(&injected.messages);

    Ok(PromptContent {
        description: injected.description,
        text,
    })
}

fn args_to_string_map(args: Option<Map<String, Value>>) -> HashMap<String, String> {
    normalize_prompt_args(args)
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| match v {
            Value::String(s) => (k, s),
            other => (k, other.to_string()),
        })
        .collect()
}

fn messages_to_text(messages: &[rmcp::model::PromptMessage]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        let role = match m.role {
            PromptMessageRole::User => "user",
            PromptMessageRole::Assistant => "assistant",
        };
        match &m.content {
            PromptMessageContent::Text { text } => {
                if messages.len() == 1 {
                    parts.push(text.clone());
                } else {
                    parts.push(format!("[{role}]\n{text}"));
                }
            }
            other => parts.push(format!("[{role}] {other:?}")),
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;
    use arc_swap::ArcSwap;
    use vmcp_notify::Bus;
    use vmcp_upstream::{ResolvedPrompt, ResolvedPromptArg};

    fn skill(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: format!("desc {name}"),
            arguments: vec![],
            template: format!("BODY {name}"),
        }
    }

    fn skill_with_arg(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: format!("desc {name}"),
            arguments: vec![crate::skills::SkillArg {
                name: "q".into(),
                description: Some("query".into()),
                required: true,
                default: None,
            }],
            template: "Q={{q}}".into(),
        }
    }

    #[test]
    fn list_local_only_when_upstream_disabled() {
        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![skill("memory_recall")]));
        let bus = Bus::new(32);
        let pool = UpstreamPool::empty_for_test(bus);
        pool.insert_synthetic_prompts_for_test(
            "tavily",
            None,
            vec![],
            vec![ResolvedPrompt {
                server: "tavily".into(),
                name: "research".into(),
                description: Some("Deep research".into()),
                arguments: vec![ResolvedPromptArg {
                    name: "topic".into(),
                    description: None,
                    required: true,
                }],
            }],
        );
        let listed = list_all(&skills, &pool, false);
        assert!(listed.iter().any(|p| p.name == "memory_recall"));
        assert!(!listed.iter().any(|p| p.name == "tavily__research"));
    }

    #[test]
    fn list_includes_upstream_when_enabled() {
        let skills: SkillsHandle =
            Arc::new(ArcSwap::from_pointee(vec![skill_with_arg("memory_recall")]));
        let bus = Bus::new(32);
        let pool = UpstreamPool::empty_for_test(bus);
        pool.insert_synthetic_prompts_for_test(
            "tavily",
            None,
            vec![],
            vec![
                ResolvedPrompt {
                    server: "tavily".into(),
                    name: "research".into(),
                    description: Some("Deep research".into()),
                    arguments: vec![ResolvedPromptArg {
                        name: "topic".into(),
                        description: Some("what to research".into()),
                        required: true,
                    }],
                },
                ResolvedPrompt {
                    server: "tavily".into(),
                    name: "brief".into(),
                    description: None, // exercises fallback description
                    arguments: vec![],
                },
            ],
        );
        let listed = list_all(&skills, &pool, true);
        let local = listed.iter().find(|p| p.name == "memory_recall").unwrap();
        assert_eq!(local.arguments.len(), 1);
        assert_eq!(local.arguments[0].description.as_deref(), Some("query"));

        let up = listed
            .iter()
            .find(|p| p.name == "tavily__research")
            .unwrap();
        assert_eq!(up.source, "upstream");
        assert_eq!(
            up.arguments[0].description.as_deref(),
            Some("what to research")
        );

        let brief = listed.iter().find(|p| p.name == "tavily__brief").unwrap();
        assert!(brief.description.contains("[tavily] brief"));
    }

    #[tokio::test]
    async fn get_local_renders_template() {
        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![skill("echo")]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        let handlers = prompt_source_handlers(skills, pool, false);
        let content = (handlers.get)("echo".into(), None).await.unwrap();
        assert_eq!(content.text, "BODY echo");
    }

    #[tokio::test]
    async fn get_local_coerces_args_and_unknown_errors() {
        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![skill_with_arg("ask")]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        let handlers = prompt_source_handlers(skills, pool, false);

        let mut args = Map::new();
        args.insert("q".into(), Value::from(42));
        let content = (handlers.get)("ask".into(), Some(args)).await.unwrap();
        assert_eq!(content.text, "Q=42");

        let err = (handlers.get)("missing".into(), None).await.unwrap_err();
        assert!(err.contains("unknown prompt"), "got: {err}");
    }

    #[tokio::test]
    async fn get_upstream_rejected_when_disabled() {
        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        pool.insert_synthetic_prompts_for_test(
            "tavily",
            None,
            vec![],
            vec![ResolvedPrompt {
                server: "tavily".into(),
                name: "research".into(),
                description: Some("Deep research".into()),
                arguments: vec![],
            }],
        );
        let handlers = prompt_source_handlers(skills, pool, false);
        let err = (handlers.get)("tavily__research".into(), None)
            .await
            .unwrap_err();
        assert!(err.contains("[proxy]"), "got: {err}");
    }

    #[tokio::test]
    async fn get_upstream_injects_routing_and_flattens_messages() {
        use rmcp::model::{GetPromptResult, PromptMessage, PromptMessageRole};
        use vmcp_registry::TaskSupportHint;
        use vmcp_upstream::ResolvedTool;

        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        pool.insert_synthetic_prompts_for_test(
            "tavily",
            None,
            vec![ResolvedTool {
                server: "tavily".into(),
                name: "tavily_search".into(),
                description: Some("search".into()),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
                task_support: TaskSupportHint::Forbidden,
            }],
            vec![ResolvedPrompt {
                server: "tavily".into(),
                name: "research".into(),
                description: Some("Deep research".into()),
                arguments: vec![],
            }],
        );
        pool.stub_prompt_get_for_test(
            "tavily",
            "research",
            GetPromptResult::new(vec![PromptMessage::new_text(
                PromptMessageRole::User,
                "Call tavily_search on the topic",
            )])
            .with_description("Deep research"),
        );

        let handlers = prompt_source_handlers(skills, pool.clone(), true);
        let mut args = Map::new();
        args.insert("topic".into(), Value::from(true));
        let content = (handlers.get)("tavily__research".into(), Some(args))
            .await
            .unwrap();
        assert!(
            content.text.contains("Query.tavily.tavilySearch"),
            "got: {}",
            content.text
        );
        assert!(content.text.contains("Call tavily_search on the topic"));
        assert_eq!(content.description.as_deref(), Some("Deep research"));
    }

    #[tokio::test]
    async fn get_upstream_formats_multi_message_bodies() {
        use rmcp::model::{GetPromptResult, PromptMessage, PromptMessageRole};

        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        pool.insert_synthetic_prompts_for_test(
            "demo",
            None,
            vec![],
            vec![ResolvedPrompt {
                server: "demo".into(),
                name: "chat".into(),
                description: Some("multi".into()),
                arguments: vec![],
            }],
        );
        pool.stub_prompt_get_for_test(
            "demo",
            "chat",
            GetPromptResult::new(vec![
                PromptMessage::new_text(PromptMessageRole::User, "hello"),
                PromptMessage::new_text(PromptMessageRole::Assistant, "world"),
            ]),
        );

        let handlers = prompt_source_handlers(skills, pool, true);
        let content = (handlers.get)("demo__chat".into(), None).await.unwrap();
        // Injection prepends into first message; multi-message flatten uses [role].
        assert!(content.text.contains("[user]") || content.text.contains("hello"));
        assert!(content.text.contains("[assistant]\nworld"));
    }

    #[tokio::test]
    async fn get_upstream_maps_pool_errors() {
        let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(vec![]));
        let bus = Bus::new(32);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        // No stub and no live client → get_prompt fails.
        pool.insert_synthetic_prompts_for_test(
            "tavily",
            None,
            vec![],
            vec![ResolvedPrompt {
                server: "tavily".into(),
                name: "research".into(),
                description: Some("Deep research".into()),
                arguments: vec![],
            }],
        );
        let handlers = prompt_source_handlers(skills, pool, true);
        let err = (handlers.get)("tavily__research".into(), None)
            .await
            .unwrap_err();
        assert!(
            err.contains("upstream `tavily` prompt `research`"),
            "got: {err}"
        );
    }

    #[test]
    fn messages_to_text_single_and_multi() {
        use rmcp::model::{PromptMessage, PromptMessageRole};
        let one = vec![PromptMessage::new_text(PromptMessageRole::User, "solo")];
        assert_eq!(messages_to_text(&one), "solo");

        let two = vec![
            PromptMessage::new_text(PromptMessageRole::User, "a"),
            PromptMessage::new_text(PromptMessageRole::Assistant, "b"),
        ];
        let out = messages_to_text(&two);
        assert_eq!(out, "[user]\na\n\n[assistant]\nb");
    }

    #[test]
    fn normalize_coerces_numbers_to_strings() {
        let mut m = Map::new();
        m.insert("limit".into(), Value::from(10));
        m.insert("q".into(), Value::String("hi".into()));
        let out = normalize_prompt_args(Some(m)).unwrap();
        assert_eq!(out.get("limit"), Some(&Value::String("10".into())));
        assert_eq!(out.get("q"), Some(&Value::String("hi".into())));
    }
}
