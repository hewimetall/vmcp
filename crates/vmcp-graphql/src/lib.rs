//! Dynamic GraphQL schema built from upstream MCP tools.
//!
//! Replaces Python `vmcp/graphql/{schema,namespaces,typed_builder,validation}.py`.
//! Schema is constructed at boot from the resolved tool catalogue (after
//! sidecar overrides). Query/Mutation bucketing is driven by `readOnlyHint`.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_graphql::dynamic::{
    Field, FieldFuture, FieldValue, InputValue, Object, Scalar, Schema, TypeRef,
};
use async_graphql::{Name, Value as GqlValue};
use serde_json::{Map, Value};

use vmcp_upstream::{ResolvedTool, UpstreamPool};

pub mod validation;

/// One prompt argument advertised in GraphQL skill discovery.
#[derive(Debug, Clone)]
pub struct PromptArgMeta {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// Lightweight prompt catalogue entry for GraphQL `prompts` / `searchPrompts`.
#[derive(Debug, Clone)]
pub struct PromptMeta {
    /// Local skill name, or `{server}__{prompt}` for upstream.
    pub name: String,
    pub description: String,
    /// `"local"` or `"upstream"`.
    pub source: String,
    pub server: Option<String>,
    pub arguments: Vec<PromptArgMeta>,
}

/// Result of GraphQL `getPrompt`.
#[derive(Debug, Clone)]
pub struct PromptContent {
    pub description: Option<String>,
    pub text: String,
}

type PromptGetFuture = Pin<Box<dyn Future<Output = Result<PromptContent, String>> + Send>>;
type PromptGetHandler = dyn Fn(String, Option<Map<String, Value>>) -> PromptGetFuture + Send + Sync;

/// Closures that back GraphQL skill/prompt discovery. Built by vmcp-server
/// (skills + upstream pool) and passed into [`build_schema`] so agents can
/// pull a playbook mid-flight via `query_graphql` alone.
#[derive(Clone)]
pub struct PromptSourceHandlers {
    pub list: Arc<dyn Fn() -> Vec<PromptMeta> + Send + Sync>,
    pub get: Arc<PromptGetHandler>,
}

/// Behaviour when an upstream response exceeds the configured byte cap.
///
/// Mirrors `vmcp_config::CapMode`. Duplicated rather than re-exported to
/// keep `vmcp-graphql` independent of the config crate's serde stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CapMode {
    #[default]
    Error,
    Truncate,
}

/// Limits enforced before queries hit resolvers.
#[derive(Debug, Clone, Copy)]
pub struct SchemaLimits {
    pub max_depth: usize,
    pub max_complexity: usize,
    /// Max bytes of upstream `text` returned to the agent. Over this, we
    /// either error (default) or truncate per `response_cap_mode`.
    pub max_response_bytes: usize,
    pub response_cap_mode: CapMode,
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_complexity: 1000,
            max_response_bytes: 1_048_576,
            response_cap_mode: CapMode::Error,
        }
    }
}

/// Build a `Schema` from the current pool catalogue.
///
/// Each upstream `foo` becomes:
///  * `FooRead` namespace type — fields for tools with `read_only = true`,
///    rooted under `Query.foo`.
///  * `FooWrite` namespace type — fields for tools with `read_only = false`,
///    rooted under `Mutation.foo`.
pub fn build_schema(
    entries: Vec<(String, Vec<ResolvedTool>)>,
    pool: Arc<UpstreamPool>,
    limits: SchemaLimits,
) -> Result<Schema, async_graphql::dynamic::SchemaError> {
    build_schema_with_prompts(entries, pool, limits, None)
}

/// Like [`build_schema`], but registers GraphQL skill discovery when
/// `prompts` handlers are provided (`prompts`, `searchPrompts`, `getPrompt`).
pub fn build_schema_with_prompts(
    entries: Vec<(String, Vec<ResolvedTool>)>,
    pool: Arc<UpstreamPool>,
    limits: SchemaLimits,
    prompts: Option<PromptSourceHandlers>,
) -> Result<Schema, async_graphql::dynamic::SchemaError> {
    let mut query = Object::new("Query");
    let mut mutation = Object::new("Mutation");

    // Scalars + static types.
    let json_scalar =
        Scalar::new("JSON").description("Arbitrary JSON. Passed verbatim to/from upstream tools.");

    let tool_result = Object::new("ToolCallResult")
        .field(Field::new(
            "isError",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<ToolCallNode>()
                    .map(|n| n.is_error)
                    .unwrap_or(true);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new("text", TypeRef::named(TypeRef::STRING), |ctx| {
            // async-graphql 7.2 made try_downcast_ref return Result instead
            // of Option — convert via .ok() before chaining .and_then on the
            // node's own Option field.
            let v = ctx
                .parent_value
                .try_downcast_ref::<ToolCallNode>()
                .ok()
                .and_then(|n| n.text.clone());
            FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
        }))
        .field(Field::new("json", TypeRef::named("JSON"), |ctx| {
            let v = ctx
                .parent_value
                .try_downcast_ref::<ToolCallNode>()
                .map(|n| n.json.clone())
                .unwrap_or(Value::Null);
            FieldFuture::new(async move { Ok(Some(FieldValue::value(json_to_gql(v)))) })
        }));

    let server_obj = Object::new("Server")
        .description(
            "Connected upstream MCP server. Operator-curated description is the cheapest \
                     discovery signal — agents can drop entire upstreams from consideration \
                     before invoking `search(q)`.",
        )
        .field(Field::new(
            "name",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<ServerNode>()
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "description",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<ServerNode>()
                    .ok()
                    .and_then(|n| n.description.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ))
        .field(Field::new(
            "toolCount",
            TypeRef::named_nn(TypeRef::INT),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<ServerNode>()
                    .map(|n| n.tool_count)
                    .unwrap_or(0);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "readOnlyCount",
            TypeRef::named_nn(TypeRef::INT),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<ServerNode>()
                    .map(|n| n.read_only_count)
                    .unwrap_or(0);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ));

    let search_hit_obj = Object::new("SearchHit")
        .description(
            "One ranked match from `search(q)`. The agent uses `server` + `tool` + \
                     `readOnly` to choose the GraphQL namespace (`<Pascal(server)><Read|Write>`) \
                     and field; introspect that namespace with `__type(name: ...)` for typed args. \
                     When `taskSupport` is `optional`/`required`, the same tool may also be \
                     invoked via the native `run_task` MCP tool (SEP-1686).",
        )
        .field(Field::new(
            "server",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<SearchHitNode>()
                    .map(|n| n.server.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "tool",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<SearchHitNode>()
                    .map(|n| n.tool.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "description",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<SearchHitNode>()
                    .ok()
                    .and_then(|n| n.description.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ))
        .field(Field::new(
            "readOnly",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<SearchHitNode>()
                    .map(|n| n.read_only)
                    .unwrap_or(false);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "taskSupport",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<SearchHitNode>()
                    .ok()
                    .and_then(|n| n.task_support.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ));

    let notification_obj = Object::new("Notification")
        .field(Field::new("id", TypeRef::named_nn(TypeRef::INT), |ctx| {
            let v = ctx
                .parent_value
                .try_downcast_ref::<NotificationNode>()
                .map(|n| n.id as i64)
                .unwrap_or(0);
            FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
        }))
        .field(Field::new(
            "source",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<NotificationNode>()
                    .map(|n| n.source.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "method",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<NotificationNode>()
                    .map(|n| n.method.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new("params", TypeRef::named_nn("JSON"), |ctx| {
            let v = ctx
                .parent_value
                .try_downcast_ref::<NotificationNode>()
                .map(|n| n.params.clone())
                .unwrap_or(Value::Null);
            FieldFuture::new(async move { Ok(Some(FieldValue::value(json_to_gql(v)))) })
        }))
        .field(Field::new(
            "tsUnixMs",
            TypeRef::named_nn(TypeRef::INT),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<NotificationNode>()
                    .map(|n| n.ts_unix_ms)
                    .unwrap_or(0);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ));

    // Root: servers — operator-curated catalog. Cheapest entry-point in the
    // discovery ladder: agent picks an upstream by purpose before reaching
    // for `search(q)`.
    {
        let pool_s = pool.clone();
        query = query.field(Field::new(
            "servers",
            TypeRef::named_nn_list_nn("Server"),
            move |_ctx| {
                let pool = pool_s.clone();
                FieldFuture::new(async move {
                    let mut nodes: Vec<ServerNode> = pool
                        .all_resolved()
                        .into_iter()
                        .map(|(name, tools)| {
                            let read_only_count =
                                tools.iter().filter(|t| t.read_only).count() as i64;
                            ServerNode {
                                description: pool.description_of(&name),
                                name: name.clone(),
                                tool_count: tools.len() as i64,
                                read_only_count,
                            }
                        })
                        .collect();
                    nodes.sort_by(|a, b| a.name.cmp(&b.name));
                    Ok(Some(FieldValue::list(
                        nodes.into_iter().map(FieldValue::owned_any),
                    )))
                })
            },
        ));
    }

    // Root: search(q: String!) — token-level lexical match across tool
    // name+description, ranked by hit count. Returns SearchHit list (no
    // score field by design — agent doesn't need it for routing).
    {
        let pool_s = pool.clone();
        query = query.field(
            Field::new(
                "search",
                TypeRef::named_nn_list_nn("SearchHit"),
                move |ctx| {
                    let pool = pool_s.clone();
                    FieldFuture::new(async move {
                        let q = ctx.args.try_get("q")?.string()?.to_lowercase();
                        let tokens: Vec<&str> = q.split_whitespace().collect();
                        if tokens.is_empty() {
                            return Ok(Some(FieldValue::list(Vec::<FieldValue>::new())));
                        }
                        let mut hits: Vec<(usize, SearchHitNode)> = Vec::new();
                        for (server, tools) in pool.all_resolved() {
                            for t in tools {
                                let hay = format!(
                                    "{} {}",
                                    t.name,
                                    t.description.as_deref().unwrap_or("")
                                )
                                .to_lowercase();
                                let score = tokens.iter().filter(|tk| hay.contains(*tk)).count();
                                if score > 0 {
                                    hits.push((
                                        score,
                                        SearchHitNode {
                                            server: server.clone(),
                                            tool: t.name.clone(),
                                            description: t.description.clone(),
                                            read_only: t.read_only,
                                            task_support: t
                                                .task_support
                                                .is_task()
                                                .then(|| t.task_support.as_str().to_string()),
                                        },
                                    ));
                                }
                            }
                        }
                        hits.sort_by(|a, b| {
                            b.0.cmp(&a.0).then_with(|| a.1.server.cmp(&b.1.server))
                        });
                        Ok(Some(FieldValue::list(
                            hits.into_iter().map(|(_, n)| FieldValue::owned_any(n)),
                        )))
                    })
                },
            )
            .argument(InputValue::new("q", TypeRef::named_nn(TypeRef::STRING))),
        );
    }

    // Root: notifications(sinceId: Int = 0, limit: Int = 100): [Notification!]!
    {
        let pool_n = pool.clone();
        query = query.field(
            Field::new(
                "notifications",
                TypeRef::named_nn_list_nn("Notification"),
                move |ctx| {
                    let pool = pool_n.clone();
                    FieldFuture::new(async move {
                        let since = ctx
                            .args
                            .try_get("sinceId")
                            .ok()
                            .and_then(|v| v.u64().ok())
                            .unwrap_or(0);
                        let limit = ctx
                            .args
                            .try_get("limit")
                            .ok()
                            .and_then(|v| v.u64().ok())
                            .unwrap_or(100) as usize;
                        let bus = pool.bus();
                        let notifs = bus.replay_since(since, limit);
                        let nodes: Vec<FieldValue> = notifs
                            .into_iter()
                            .map(|n| {
                                FieldValue::owned_any(NotificationNode {
                                    id: n.id,
                                    source: n.source.clone(),
                                    method: n.method.clone(),
                                    params: n.params.clone(),
                                    ts_unix_ms: n.ts_unix_ms,
                                })
                            })
                            .collect();
                        Ok(Some(FieldValue::list(nodes)))
                    })
                },
            )
            .argument(
                InputValue::new("sinceId", TypeRef::named(TypeRef::INT))
                    .default_value(GqlValue::from(0)),
            )
            .argument(
                InputValue::new("limit", TypeRef::named(TypeRef::INT))
                    .default_value(GqlValue::from(100)),
            ),
        );
    }

    // Skill / prompt discovery — optional handlers from vmcp-server so the
    // agent can list and pull playbooks mid-flight via query_graphql alone.
    let prompt_arg_obj = Object::new("PromptArg")
        .field(Field::new(
            "name",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptArgNode>()
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "description",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptArgNode>()
                    .ok()
                    .and_then(|n| n.description.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ))
        .field(Field::new(
            "required",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptArgNode>()
                    .map(|n| n.required)
                    .unwrap_or(false);
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ));

    let prompt_obj = Object::new("Prompt")
        .description(
            "Skill / prompt playbook. Local YAML skills use bare names; upstream \
             prompts (`{server}__{name}`) appear when `[proxy]` is enabled. \
             Fetch full instructions with `getPrompt`.",
        )
        .field(Field::new(
            "name",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptNode>()
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "description",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptNode>()
                    .map(|n| n.description.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "source",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptNode>()
                    .map(|n| n.source.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ))
        .field(Field::new(
            "server",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptNode>()
                    .ok()
                    .and_then(|n| n.server.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ))
        .field(Field::new(
            "arguments",
            TypeRef::named_nn_list_nn("PromptArg"),
            |ctx| {
                let args = ctx
                    .parent_value
                    .try_downcast_ref::<PromptNode>()
                    .map(|n| n.arguments.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move {
                    Ok(Some(FieldValue::list(
                        args.into_iter().map(FieldValue::owned_any),
                    )))
                })
            },
        ));

    let prompt_content_obj = Object::new("PromptContent")
        .description(
            "Rendered prompt body. Upstream prompts are prepended with a GraphQL \
             tool-routing table — call tools via query_graphql only.",
        )
        .field(Field::new(
            "description",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptContentNode>()
                    .ok()
                    .and_then(|n| n.description.clone());
                FieldFuture::new(async move { Ok(v.map(FieldValue::value)) })
            },
        ))
        .field(Field::new(
            "text",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                let v = ctx
                    .parent_value
                    .try_downcast_ref::<PromptContentNode>()
                    .map(|n| n.text.clone())
                    .unwrap_or_default();
                FieldFuture::new(async move { Ok(Some(FieldValue::value(v))) })
            },
        ));

    if let Some(handlers) = prompts.clone() {
        let list_h = handlers.list.clone();
        query = query.field(Field::new(
            "prompts",
            TypeRef::named_nn_list_nn("Prompt"),
            move |_ctx| {
                let list_h = list_h.clone();
                FieldFuture::new(async move {
                    let nodes: Vec<PromptNode> =
                        list_h().into_iter().map(prompt_meta_to_node).collect();
                    Ok(Some(FieldValue::list(
                        nodes.into_iter().map(FieldValue::owned_any),
                    )))
                })
            },
        ));

        let list_h = handlers.list.clone();
        query = query.field(
            Field::new(
                "searchPrompts",
                TypeRef::named_nn_list_nn("Prompt"),
                move |ctx| {
                    let list_h = list_h.clone();
                    FieldFuture::new(async move {
                        let q = ctx.args.try_get("q")?.string()?.to_lowercase();
                        let tokens: Vec<&str> = q.split_whitespace().collect();
                        if tokens.is_empty() {
                            return Ok(Some(FieldValue::list(Vec::<FieldValue>::new())));
                        }
                        let mut scored: Vec<(usize, PromptNode)> = Vec::new();
                        for meta in list_h() {
                            let hay = format!("{} {}", meta.name, meta.description).to_lowercase();
                            let hits = tokens.iter().filter(|t| hay.contains(**t)).count();
                            if hits > 0 {
                                scored.push((hits, prompt_meta_to_node(meta)));
                            }
                        }
                        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
                        Ok(Some(FieldValue::list(
                            scored.into_iter().map(|(_, n)| FieldValue::owned_any(n)),
                        )))
                    })
                },
            )
            .argument(InputValue::new("q", TypeRef::named_nn(TypeRef::STRING))),
        );

        let get_h = handlers.get.clone();
        query = query.field(
            Field::new(
                "getPrompt",
                TypeRef::named_nn("PromptContent"),
                move |ctx| {
                    let get_h = get_h.clone();
                    FieldFuture::new(async move {
                        let name = ctx.args.try_get("name")?.string()?.to_string();
                        let args = match ctx.args.try_get("arguments") {
                            Ok(v) => {
                                let gql = v.deserialize::<GqlValue>()?;
                                Some(gql_object_to_map(gql)?)
                            }
                            Err(_) => None,
                        };
                        match get_h(name, args).await {
                            Ok(content) => Ok(Some(FieldValue::owned_any(PromptContentNode {
                                description: content.description,
                                text: content.text,
                            }))),
                            Err(e) => Err(async_graphql::Error::new(e)),
                        }
                    })
                },
            )
            .argument(InputValue::new("name", TypeRef::named_nn(TypeRef::STRING)))
            .argument(InputValue::new("arguments", TypeRef::named("JSON"))),
        );
    }

    let mut builder = Schema::build("Query", Some("Mutation"), None)
        .register(json_scalar)
        .register(tool_result)
        .register(server_obj)
        .register(search_hit_obj)
        .register(notification_obj)
        .register(prompt_arg_obj)
        .register(prompt_obj)
        .register(prompt_content_obj)
        .limit_depth(limits.max_depth)
        .limit_complexity(limits.max_complexity);

    let mut had_mutation_ns = false;

    // Per-upstream namespaces.
    for (server, tools) in entries {
        if tools.is_empty() {
            continue;
        }
        let (reads, writes): (Vec<_>, Vec<_>) = tools.into_iter().partition(|t| t.read_only);

        if !reads.is_empty() {
            let obj = build_namespace_object(
                &server,
                "Read",
                &reads,
                pool.clone(),
                limits.max_response_bytes,
                limits.response_cap_mode,
            );
            let type_name = format!("{}Read", pascal_case(&server));
            builder = builder.register(obj);
            let field_name = camel_case(&server);
            query = query.field(Field::new(
                field_name,
                TypeRef::named_nn(type_name),
                |_ctx| FieldFuture::new(async { Ok(Some(FieldValue::owned_any(NamespaceMarker))) }),
            ));
        }

        if !writes.is_empty() {
            let obj = build_namespace_object(
                &server,
                "Write",
                &writes,
                pool.clone(),
                limits.max_response_bytes,
                limits.response_cap_mode,
            );
            let type_name = format!("{}Write", pascal_case(&server));
            builder = builder.register(obj);
            let field_name = camel_case(&server);
            mutation = mutation.field(Field::new(
                field_name,
                TypeRef::named_nn(type_name),
                |_ctx| FieldFuture::new(async { Ok(Some(FieldValue::owned_any(NamespaceMarker))) }),
            ));
            had_mutation_ns = true;
        }
    }

    // GraphQL requires every Object type to have at least one field.
    if !had_mutation_ns {
        mutation = mutation.field(Field::new(
            "_unavailable",
            TypeRef::named(TypeRef::BOOLEAN),
            |_| FieldFuture::new(async { Ok(Some(FieldValue::value(false))) }),
        ));
    }

    builder.register(query).register(mutation).finish()
}

/// Marker for namespace fields whose children resolve themselves.
struct NamespaceMarker;

#[derive(Debug, Clone)]
struct ToolCallNode {
    is_error: bool,
    text: Option<String>,
    json: Value,
}

#[derive(Debug, Clone)]
struct NotificationNode {
    id: u64,
    source: String,
    method: String,
    params: Value,
    ts_unix_ms: i64,
}

#[derive(Debug, Clone)]
struct ServerNode {
    name: String,
    description: Option<String>,
    tool_count: i64,
    read_only_count: i64,
}

#[derive(Debug, Clone)]
struct SearchHitNode {
    server: String,
    tool: String,
    description: Option<String>,
    read_only: bool,
    /// `optional` / `required` when the tool is on the `run_task` allowlist; None otherwise.
    task_support: Option<String>,
}

#[derive(Debug, Clone)]
struct PromptArgNode {
    name: String,
    description: Option<String>,
    required: bool,
}

#[derive(Debug, Clone)]
struct PromptNode {
    name: String,
    description: String,
    source: String,
    server: Option<String>,
    arguments: Vec<PromptArgNode>,
}

#[derive(Debug, Clone)]
struct PromptContentNode {
    description: Option<String>,
    text: String,
}

fn prompt_meta_to_node(meta: PromptMeta) -> PromptNode {
    PromptNode {
        name: meta.name,
        description: meta.description,
        source: meta.source,
        server: meta.server,
        arguments: meta
            .arguments
            .into_iter()
            .map(|a| PromptArgNode {
                name: a.name,
                description: a.description,
                required: a.required,
            })
            .collect(),
    }
}

fn gql_object_to_map(v: GqlValue) -> Result<Map<String, Value>, async_graphql::Error> {
    match v {
        GqlValue::Object(obj) => {
            let mut out = Map::new();
            for (k, val) in obj {
                out.insert(k.to_string(), gql_to_json(&val));
            }
            Ok(out)
        }
        GqlValue::Null => Ok(Map::new()),
        other => Err(async_graphql::Error::new(format!(
            "arguments must be a JSON object, got {other:?}"
        ))),
    }
}

fn build_namespace_object(
    server: &str,
    suffix: &str, // "Read" or "Write"
    tools: &[ResolvedTool],
    pool: Arc<UpstreamPool>,
    max_response_bytes: usize,
    cap_mode: CapMode,
) -> Object {
    let ns_name = format!("{}{}", pascal_case(server), suffix);
    let mut ns = Object::new(&ns_name);

    for tool in tools {
        let field_name = camel_case(&tool.name);
        let upstream_server = tool.server.clone();
        let upstream_tool_name = tool.name.clone();
        let pool_for_tool = pool.clone();

        // `args_map` is the inverse of the snake→camel transform we do in
        // `build_field_args`: at resolve time we get camelCase arg names from
        // the GQL context and need to ship the original snake_case keys back
        // upstream (otherwise json-schema `required` checks fail).
        let (arg_specs, args_map_for_tool) = match build_field_args(&tool.input_schema) {
            Some((specs, map)) => (Some(specs), Arc::new(map)),
            None => (None, Arc::new(HashMap::new())),
        };

        let mut field = Field::new(
            field_name,
            TypeRef::named_nn("ToolCallResult"),
            move |ctx| {
                let server = upstream_server.clone();
                let tool = upstream_tool_name.clone();
                let pool = pool_for_tool.clone();
                let args_map = args_map_for_tool.clone();
                let args_json = collect_args(&ctx, &args_map);
                FieldFuture::new(async move {
                    let res = pool.call(&server, &tool, args_json).await;
                    let node = match res {
                        Ok(r) => result_to_node(r, max_response_bytes, cap_mode),
                        Err(e) => ToolCallNode {
                            is_error: true,
                            text: Some(format!("{e:#}")),
                            json: Value::Null,
                        },
                    };
                    Ok(Some(FieldValue::owned_any(node)))
                })
            },
        );

        if let Some(desc) = &tool.description {
            field = field.description(desc.clone());
        }

        if let Some(args) = arg_specs {
            for (arg_name, arg_type, default) in args {
                let mut iv = InputValue::new(arg_name, arg_type);
                if let Some(d) = default {
                    iv = iv.default_value(d);
                }
                field = field.argument(iv);
            }
        }

        ns = ns.field(field);
    }

    ns
}

/// Convert a tool's JSON Schema into a list of (name, type, default) tuples
/// and a `camelCase → original` map for the inverse rename at resolve time.
#[allow(clippy::type_complexity)]
fn build_field_args(
    schema: &Value,
) -> Option<(
    Vec<(String, TypeRef, Option<GqlValue>)>,
    HashMap<String, String>,
)> {
    let obj = schema.as_object()?;
    let props = obj.get("properties").and_then(|v| v.as_object())?;
    let required: Vec<&str> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut map: HashMap<String, String> = HashMap::new();
    for (name, sub) in props {
        let is_required = required.iter().any(|r| r == name);
        let t = json_schema_to_gql_type(sub, is_required);
        let default = sub.get("default").map(|v| json_to_gql(v.clone()));
        let camel = camel_case(name);
        if let Some(prev) = map.insert(camel.clone(), name.clone()) {
            // Two distinct upstream properties collide on camel-case (would only
            // happen with e.g. `foo_bar` + `fooBar`); we last-write-wins so the
            // GQL field still works, but log loud enough to be investigable.
            tracing::warn!(
                camel = %camel,
                previous = %prev,
                current = %name,
                "build_field_args: camelCase collision; last-write-wins"
            );
        }
        out.push((camel, t, default));
    }
    Some((out, map))
}

/// Map a JSON-schema fragment to a GraphQL type ref. Falls back to `JSON`
/// scalar for anything the dumb mapper can't handle.
fn json_schema_to_gql_type(schema: &Value, required: bool) -> TypeRef {
    let Some(obj) = schema.as_object() else {
        return type_or_nn("JSON", required);
    };

    // Anything with composition keywords degrades to JSON.
    if obj.contains_key("oneOf")
        || obj.contains_key("anyOf")
        || obj.contains_key("allOf")
        || obj.contains_key("$ref")
    {
        return type_or_nn("JSON", required);
    }

    let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "string" => type_or_nn(TypeRef::STRING, required),
        "integer" => type_or_nn(TypeRef::INT, required),
        "number" => type_or_nn(TypeRef::FLOAT, required),
        "boolean" => type_or_nn(TypeRef::BOOLEAN, required),
        "array" => {
            let inner_name = obj
                .get("items")
                .and_then(|i| i.as_object())
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                .and_then(|t| match t {
                    "string" => Some(TypeRef::STRING),
                    "integer" => Some(TypeRef::INT),
                    "number" => Some(TypeRef::FLOAT),
                    "boolean" => Some(TypeRef::BOOLEAN),
                    _ => None,
                })
                .unwrap_or("JSON");
            if required {
                TypeRef::named_nn_list_nn(inner_name)
            } else {
                TypeRef::named_nn_list(inner_name)
            }
        }
        _ => type_or_nn("JSON", required),
    }
}

fn type_or_nn(name: &'static str, required: bool) -> TypeRef {
    if required {
        TypeRef::named_nn(name)
    } else {
        TypeRef::named(name)
    }
}

/// Collect the resolver's args into a JSON object, undoing the
/// snake→camel rename done in `build_field_args` before shipping to upstream.
fn collect_args(
    ctx: &async_graphql::dynamic::ResolverContext<'_>,
    args_map: &HashMap<String, String>,
) -> Value {
    collect_args_from_pairs(
        ctx.args.iter().map(|(n, v)| {
            // async-graphql 7.2: iter() yields ValueAccessor — unwrap to &Value
            // and clone so the helper can be ResolverContext-agnostic.
            (n.to_string(), gql_to_json(v.as_value()))
        }),
        args_map,
    )
}

/// Pure helper: builds the upstream-bound JSON object from already-resolved
/// `(camelCase_name, json_value)` pairs. Split out so unit tests can exercise
/// the rename logic without constructing an async-graphql `ResolverContext`.
fn collect_args_from_pairs<I>(pairs: I, args_map: &HashMap<String, String>) -> Value
where
    I: IntoIterator<Item = (String, Value)>,
{
    let mut out = Map::new();
    for (camel_key, json) in pairs {
        if json.is_null() {
            continue;
        }
        let key = args_map
            .get(camel_key.as_str())
            .cloned()
            .unwrap_or(camel_key);
        out.insert(key, json);
    }
    Value::Object(out)
}

fn result_to_node(
    r: rmcp::model::CallToolResult,
    max_bytes: usize,
    cap_mode: CapMode,
) -> ToolCallNode {
    let is_error = r.is_error.unwrap_or(false);
    let text = r
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Prefer MCP `structuredContent` when present (image/audio tools often
    // return no text blocks — only structured metadata + binary content).
    let structured = r.structured_content.clone();

    if text.len() > max_bytes {
        return match cap_mode {
            CapMode::Error => ToolCallNode {
                is_error: true,
                text: Some(format!(
                    "response too large: {} bytes > cap {} (paginate or narrow your query)",
                    text.len(),
                    max_bytes
                )),
                json: serde_json::json!({
                    "_truncated": true,
                    "_original_bytes": text.len(),
                    "_cap_bytes": max_bytes,
                }),
            },
            CapMode::Truncate => {
                // Slice on a char boundary so we don't split a multibyte
                // sequence. We bound by `max_bytes` chars (over-conservative
                // for non-ASCII, but never returns more bytes than the cap).
                let prefix: String = text.chars().take(max_bytes).collect();
                ToolCallNode {
                    is_error: false,
                    text: Some(prefix.clone()),
                    json: serde_json::json!({
                        "_truncated": true,
                        "_original_bytes": text.len(),
                        "_cap_bytes": max_bytes,
                        "_data_prefix": prefix,
                    }),
                }
            }
        };
    }

    let json: Value = if !text.is_empty() {
        // Text payload remains the primary GraphQL `json` projection for
        // ordinary tools (FastMCP often also mirrors it under structuredContent
        // with an extra wrapper — keep the text-parsed shape stable).
        serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()))
    } else if let Some(sc) = structured {
        // Image/audio tools: no text blocks, only structuredContent (+ binary).
        sc
    } else {
        // No text and no structuredContent — surface non-text content kinds
        // so agents see *something* instead of a null ToolCallResult.
        let kinds: Vec<&str> = r
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                rmcp::model::RawContent::Image(_) => Some("image"),
                rmcp::model::RawContent::Audio(_) => Some("audio"),
                rmcp::model::RawContent::Resource(_) => Some("resource"),
                rmcp::model::RawContent::ResourceLink(_) => Some("resource_link"),
                rmcp::model::RawContent::Text(_) => None,
            })
            .collect();
        if kinds.is_empty() {
            Value::Null
        } else {
            serde_json::json!({ "_contentKinds": kinds, "_note": "binary/non-text content not projected into GraphQL; use /mcp-proxy for raw bytes" })
        }
    };

    ToolCallNode {
        is_error,
        text: if text.is_empty() { None } else { Some(text) },
        json,
    }
}

pub fn pascal_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper_next = true;
    for ch in s.chars() {
        if ch == '_' || ch == '-' || ch.is_whitespace() {
            upper_next = true;
            continue;
        }
        if upper_next {
            for u in ch.to_uppercase() {
                out.push(u);
            }
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn camel_case(s: &str) -> String {
    let p = pascal_case(s);
    let mut chars = p.chars();
    match chars.next() {
        Some(c) => {
            let mut out = String::with_capacity(p.len());
            for l in c.to_lowercase() {
                out.push(l);
            }
            out.extend(chars);
            out
        }
        None => String::new(),
    }
}

fn json_to_gql(v: Value) -> GqlValue {
    match v {
        Value::Null => GqlValue::Null,
        Value::Bool(b) => GqlValue::Boolean(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                GqlValue::Number(async_graphql::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                async_graphql::Number::from_f64(f)
                    .map(GqlValue::Number)
                    .unwrap_or(GqlValue::Null)
            } else {
                GqlValue::Null
            }
        }
        Value::String(s) => GqlValue::String(s),
        Value::Array(a) => GqlValue::List(a.into_iter().map(json_to_gql).collect()),
        Value::Object(o) => {
            let mut map = async_graphql::indexmap::IndexMap::new();
            for (k, v) in o {
                map.insert(Name::new(k), json_to_gql(v));
            }
            GqlValue::Object(map)
        }
    }
}

fn gql_to_json(v: &GqlValue) -> Value {
    match v {
        GqlValue::Null => Value::Null,
        GqlValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        GqlValue::String(s) => Value::String(s.clone()),
        GqlValue::Boolean(b) => Value::Bool(*b),
        GqlValue::Binary(_) => Value::Null,
        GqlValue::Enum(e) => Value::String(e.to_string()),
        GqlValue::List(items) => Value::Array(items.iter().map(gql_to_json).collect()),
        GqlValue::Object(o) => {
            let mut m = Map::new();
            for (k, v) in o {
                m.insert(k.to_string(), gql_to_json(v));
            }
            Value::Object(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pascal_camel_basic() {
        assert_eq!(pascal_case("rust_demo"), "RustDemo");
        assert_eq!(pascal_case("rust-demo"), "RustDemo");
        assert_eq!(camel_case("rust_demo"), "rustDemo");
        assert_eq!(camel_case("query_graphql"), "queryGraphql");
        assert_eq!(camel_case("a"), "a");
    }

    #[test]
    fn json_schema_string_required_renders_nonnull() {
        let s = json!({"type": "string"});
        let t = json_schema_to_gql_type(&s, true);
        // Use Display impl: NonNull renders as "Name!".
        assert_eq!(t.to_string(), "String!");
    }

    #[test]
    fn json_schema_oneof_falls_back_to_json() {
        let s = json!({"oneOf": [{"type":"string"},{"type":"integer"}]});
        let t = json_schema_to_gql_type(&s, false);
        assert_eq!(t.to_string(), "JSON");
    }

    #[test]
    fn json_schema_array_of_strings() {
        let s = json!({"type": "array", "items": {"type": "string"}});
        let t = json_schema_to_gql_type(&s, false);
        // Required-false outer list: [String!]
        assert_eq!(t.to_string(), "[String!]");
    }

    #[test]
    fn json_schema_required_array() {
        let s = json!({"type": "array", "items": {"type": "integer"}});
        let t = json_schema_to_gql_type(&s, true);
        // Required outer list: [Int!]!
        assert_eq!(t.to_string(), "[Int!]!");
    }

    // ---------- build_field_args: camel-map round-trip ----------

    #[test]
    fn build_field_args_emits_inverse_map_for_snake_props() {
        let s = json!({
            "type": "object",
            "properties": {
                "source_timezone": {"type": "string"},
                "target_timezone": {"type": "string"},
                "time": {"type": "string"}
            },
            "required": ["source_timezone", "target_timezone"]
        });
        let (specs, map) = build_field_args(&s).expect("schema with properties");

        // Three args, all camel-cased.
        let names: Vec<&str> = specs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"sourceTimezone"));
        assert!(names.contains(&"targetTimezone"));
        assert!(names.contains(&"time"));

        // Map reverses camel → snake for the ones that changed.
        assert_eq!(
            map.get("sourceTimezone").map(String::as_str),
            Some("source_timezone")
        );
        assert_eq!(
            map.get("targetTimezone").map(String::as_str),
            Some("target_timezone")
        );
        // Identity entry — even non-renamed props are present in the map.
        assert_eq!(map.get("time").map(String::as_str), Some("time"));
    }

    #[test]
    fn build_field_args_identity_for_already_camel() {
        let s = json!({
            "type": "object",
            "properties": {
                "clientInfo": {"type": "string"}
            }
        });
        let (_, map) = build_field_args(&s).expect("schema with properties");
        assert_eq!(
            map.get("clientInfo").map(String::as_str),
            Some("clientInfo")
        );
    }

    #[test]
    fn build_field_args_returns_none_without_properties() {
        let s = json!({"type": "object"});
        assert!(build_field_args(&s).is_none());
    }

    // ---------- collect_args_from_pairs: the rename actually fires ----------

    #[test]
    fn collect_args_renames_camel_to_snake() {
        // Simulating the BUG-03 fix path: the agent calls
        // `convertTime(sourceTimezone: ...)` and the upstream wants
        // `source_timezone` back.
        let mut map = HashMap::new();
        map.insert("sourceTimezone".to_string(), "source_timezone".to_string());
        map.insert("targetTimezone".to_string(), "target_timezone".to_string());
        map.insert("time".to_string(), "time".to_string());

        let pairs = vec![
            (
                "sourceTimezone".to_string(),
                Value::String("Europe/Amsterdam".into()),
            ),
            (
                "targetTimezone".to_string(),
                Value::String("America/New_York".into()),
            ),
            ("time".to_string(), Value::String("15:00".into())),
        ];
        let out = collect_args_from_pairs(pairs, &map);
        let obj = out.as_object().unwrap();
        assert_eq!(
            obj.get("source_timezone").unwrap(),
            &json!("Europe/Amsterdam")
        );
        assert_eq!(
            obj.get("target_timezone").unwrap(),
            &json!("America/New_York")
        );
        assert_eq!(obj.get("time").unwrap(), &json!("15:00"));
        // No leaked camel key.
        assert!(!obj.contains_key("sourceTimezone"));
        assert!(!obj.contains_key("targetTimezone"));
    }

    #[test]
    fn collect_args_identity_for_already_camel_in_map() {
        let mut map = HashMap::new();
        map.insert("clientInfo".to_string(), "clientInfo".to_string());

        let pairs = vec![("clientInfo".to_string(), Value::String("v".into()))];
        let out = collect_args_from_pairs(pairs, &map);
        assert_eq!(
            out.as_object().unwrap().get("clientInfo").unwrap(),
            &json!("v")
        );
    }

    #[test]
    fn collect_args_unknown_key_falls_back_to_identity() {
        // Defensive path: if the schema didn't list a property (e.g. agent
        // smuggled `__typename`), we ship it through unchanged rather than
        // dropping or panicking.
        let map = HashMap::new();
        let pairs = vec![(
            "__typename".to_string(),
            Value::String("ToolCallResult".into()),
        )];
        let out = collect_args_from_pairs(pairs, &map);
        assert_eq!(
            out.as_object().unwrap().get("__typename").unwrap(),
            &json!("ToolCallResult")
        );
    }

    #[test]
    fn collect_args_drops_null_values() {
        let map = HashMap::new();
        let pairs = vec![
            ("a".to_string(), Value::String("kept".into())),
            ("b".to_string(), Value::Null),
        ];
        let out = collect_args_from_pairs(pairs, &map);
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("a"));
        assert!(!obj.contains_key("b"));
    }

    // ---------- result_to_node: cap modes ----------

    fn mk_result(text: &str) -> rmcp::model::CallToolResult {
        rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(text.to_string())])
    }

    #[test]
    fn result_to_node_under_cap_passes_through() {
        let r = mk_result(r#"{"ok": true}"#);
        let node = result_to_node(r, 100, CapMode::Error);
        assert!(!node.is_error);
        assert_eq!(node.text.as_deref(), Some(r#"{"ok": true}"#));
        assert_eq!(node.json, json!({"ok": true}));
    }

    #[test]
    fn result_to_node_exact_cap_passes_through() {
        // Boundary: text.len() == cap → NOT truncated (strict `>` check).
        let s = "x".repeat(50);
        let r = mk_result(&s);
        let node = result_to_node(r, 50, CapMode::Error);
        assert!(!node.is_error);
        assert_eq!(node.text.as_deref(), Some(s.as_str()));
    }

    #[test]
    fn result_to_node_over_cap_error_mode() {
        let s = "x".repeat(2_000);
        let r = mk_result(&s);
        let node = result_to_node(r, 100, CapMode::Error);
        assert!(node.is_error, "error mode must set isError=true");
        let text = node.text.expect("explanatory message");
        assert!(text.contains("response too large"), "got: {}", text);
        assert!(
            text.contains("2000"),
            "must mention original size, got: {}",
            text
        );
        assert!(text.contains("100"), "must mention cap, got: {}", text);
        // json carries structured metadata, but no payload.
        assert_eq!(node.json["_truncated"], json!(true));
        assert_eq!(node.json["_original_bytes"], json!(2_000));
        assert_eq!(node.json["_cap_bytes"], json!(100));
        assert!(
            node.json.get("_data_prefix").is_none(),
            "error mode must NOT include data"
        );
    }

    #[test]
    fn result_to_node_over_cap_truncate_mode() {
        let s = "x".repeat(2_000);
        let r = mk_result(&s);
        let node = result_to_node(r, 100, CapMode::Truncate);
        assert!(!node.is_error, "truncate mode must keep isError=false");
        let prefix = node.text.expect("truncated text");
        assert_eq!(prefix.len(), 100);
        assert_eq!(node.json["_truncated"], json!(true));
        assert_eq!(node.json["_original_bytes"], json!(2_000));
        assert_eq!(node.json["_cap_bytes"], json!(100));
        assert_eq!(
            node.json["_data_prefix"].as_str().map(|s| s.len()),
            Some(100)
        );
    }

    #[test]
    fn result_to_node_empty_text_is_null_json() {
        let r = rmcp::model::CallToolResult::success(vec![]);
        let node = result_to_node(r, 1024, CapMode::Error);
        assert!(!node.is_error);
        assert!(node.text.is_none());
        assert_eq!(node.json, Value::Null);
    }

    #[test]
    fn result_to_node_prefers_structured_content_when_no_text() {
        // Image-only tools often return structuredContent with no text blocks.
        let mut r = rmcp::model::CallToolResult::success(vec![]);
        r.structured_content = Some(json!({
            "slide": 1,
            "format": "png",
            "path": "/tmp/slide.001.png"
        }));
        let node = result_to_node(r, 1024, CapMode::Error);
        assert!(!node.is_error);
        assert!(node.text.is_none());
        assert_eq!(node.json["slide"], json!(1));
        assert_eq!(node.json["format"], json!("png"));
    }

    #[test]
    fn result_to_node_text_json_preferred_when_present() {
        // When text is present it stays authoritative for `json` (stable shape).
        let r = rmcp::model::CallToolResult::structured(json!({"slide": 2}));
        let node = result_to_node(r, 1024, CapMode::Error);
        assert!(!node.is_error);
        assert!(node.text.is_some());
        // `structured()` duplicates value into text as a JSON string — parse it.
        assert_eq!(node.json["slide"], json!(2));
    }
}
