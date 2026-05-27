//! The MCP server surface: one tool (`query_graphql`) plus operator-curated
//! prompts (skills).
//!
//! Replaces Python `vmcp/mcp_app.py` + `tools/query_graphql.py`. Code Mode
//! (execute_code / get_code_result / Dagger sandbox) is intentionally not
//! reimplemented — see the presentation for the talk-level justification.
//!
//! The two MCP surfaces this server exposes:
//!
//! * **Tool** — `query_graphql` is the sole tool. Discovery happens inside
//!   the GraphQL schema (`servers`, `search`, `__type`); the tool's
//!   description spells out the lazy ladder.
//! * **Prompts** — operator-authored YAML files in `skills_dir` become MCP
//!   prompts via `prompts/list` and `prompts/get`. See [`skills`].

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_graphql::dynamic::Schema;
use async_graphql::Request;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};

use vmcp_upstream::UpstreamPool;

pub mod skills;
pub use skills::{delete_skill, load_skills, render_skill, save_skill, Skill, SkillArg};

pub mod sessions;
pub mod recorder;

pub mod proxy;
pub use proxy::ProxyServer;

/// Hot-swappable GraphQL schema handle. The bin holds the canonical
/// `Arc<ArcSwap<Schema>>`; vmcp-server clones the same Arc so a swap done
/// by the drift-handler in the bin is visible to in-flight tool calls
/// here.
pub type SchemaHandle = Arc<ArcSwap<Schema>>;

/// Hot-swappable skills handle. The admin API mutates the on-disk yaml
/// files, reloads `Vec<Skill>` from disk, and `.store()`s the new pointee
/// into the same Arc that VmcpServer is holding — so `prompts/list` and
/// `prompts/get` see the change without restarting the server.
pub type SkillsHandle = Arc<ArcSwap<Vec<Skill>>>;

#[derive(Clone)]
pub struct VmcpServer {
    inner: Arc<Inner>,
    tool_router: ToolRouter<VmcpServer>,
}

struct Inner {
    schema: SchemaHandle,
    pool: Arc<UpstreamPool>,
    /// Operator-authored skills. Hot-swappable: admin CRUD writes the YAML
    /// file, reloads from disk, then swaps the pointee. `prompts/list` and
    /// `prompts/get` re-read on every call so changes go live without a
    /// gateway restart.
    skills: SkillsHandle,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryGraphqlArgs {
    /// GraphQL query document.
    pub query: String,
    /// Optional variables map.
    #[serde(default)]
    pub variables: Option<Value>,
    /// Optional operation name when the document defines several.
    #[serde(default)]
    pub operation_name: Option<String>,
}

#[tool_router]
impl VmcpServer {
    pub fn new(schema: SchemaHandle, pool: Arc<UpstreamPool>, skills: SkillsHandle) -> Self {
        Self {
            inner: Arc::new(Inner { schema, pool, skills }),
            tool_router: Self::tool_router(),
        }
    }

    /// Replace the active schema. Called when an upstream's tools/list drift
    /// triggers a rebuild.
    pub fn swap_schema(&self, schema: Schema) {
        self.inner.schema.store(Arc::new(schema));
    }

    /// Schema handle clone — useful for the bin to install drift callbacks.
    pub fn schema_handle(&self) -> SchemaHandle {
        self.inner.schema.clone()
    }

    #[tool(
        description = "\
Execute GraphQL against the vmcp semantic layer. This is the ONLY tool — every upstream MCP \
server (postgres, time, jira, etc.) is reached through ONE typed GraphQL surface, NOT through \
separate per-upstream tools.\n\
\n\
═══════════════════════════════════════════════════════════════\n\
RULE #1 — BATCH EVERYTHING INTO ONE CALL.\n\
═══════════════════════════════════════════════════════════════\n\
\n\
GraphQL aliased fields execute in PARALLEL inside a single document. The whole point of this \
tool is that you write ONE document covering EVERY piece of data the user asked for, and the \
server fans out concurrently. Do not call `query_graphql` multiple times in a row for related \
questions — that is the single biggest waste of tokens and round-trips you can do here.\n\
\n\
❌ ANTI-PATTERN (do NOT do this):\n\
  turn 1: query_graphql({ time { getCurrentTime(timezone: \"Europe/Moscow\") { json } } })\n\
  turn 2: query_graphql({ time { getCurrentTime(timezone: \"Asia/Tokyo\") { json } } })\n\
  turn 3: query_graphql({ postgres { query(sql: \"SELECT ...\") { json } } })\n\
\n\
✓ CORRECT (one call, three aliased fields run concurrently):\n\
  query_graphql({\n\
    moscow:    time { getCurrentTime(timezone: \"Europe/Moscow\") { json } }\n\
    tokyo:     time { getCurrentTime(timezone: \"Asia/Tokyo\")    { json } }\n\
    customers: postgres { query(sql: \"SELECT name, country FROM customers\") { json } }\n\
  })\n\
\n\
Multiple top-level aliases in one document = one network round-trip = one entry in the audit \
log. The user's question may MENTION several things — count them, alias them all in ONE \
document, send it once. If you find yourself thinking \"first I'll fetch X, then Y\" — stop, \
combine them.\n\
\n\
Aliasing inside a single SQL: use SQL features (UNION ALL, JOIN, CASE) to pack multiple \
related questions into one `postgres.query` field. Even cheaper than two GraphQL aliases.\n\
\n\
═══════════════════════════════════════════════════════════════\n\
DISCOVERY LADDER (only when you don't already know the field names)\n\
═══════════════════════════════════════════════════════════════\n\
\n\
Start at the cheapest step. Skip steps you don't need. Then write ONE batched call.\n\
\n\
  0. (MCP) `prompts/list` + `prompts/get` — operator-curated skill playbooks. If a prompt \
     description matches the task, fetch it: the rendered template already contains a \
     ready-made aliased GraphQL document. Use it VERBATIM — do not split it up.\n\
  1. `{ servers { name description toolCount readOnlyCount } }` — catalogue of upstreams. \
     Cheapest GraphQL probe. Combine with the real query in one document if you want both.\n\
  2. `{ search(q: \"<keywords>\") { server tool readOnly description } }` — token-level \
     full-text match across tool names + descriptions, ranked. Token match is \
     case-insensitive, no fuzzy/synonyms — pick descriptive keywords.\n\
  3. `{ ns: __type(name: \"<Pascal(server)><Read|Write>\") { fields { name description \
     args { name description type { kind name ofType { kind name ofType { kind name }}}}}}}` — \
     typed signature for ONE namespace. Name is `PascalCase(server)` + `Read` if `readOnly` \
     is true else `Write`.\n\
  4. Compose the real call as ONE aliased document.\n\
\n\
═══════════════════════════════════════════════════════════════\n\
SHAPE & RULES\n\
═══════════════════════════════════════════════════════════════\n\
\n\
  Query    `{ <serverCamel> { <toolCamel>(args) { json text isError } } }`\n\
  Mutation `mutation { <serverCamel> { <toolCamel>(args) { json text isError } } }`\n\
  Read vs Write split: by upstream's `readOnlyHint`. search/list/get → Query.foo, \
  create/update/delete → Mutation.foo.\n\
\n\
- DO NOT request `__schema { types { ... fields { ... } ... } }` with deep nesting — dumps \
  the entire catalogue and burns context. Shallow `__schema { types { name kind } }` ok as \
  a fallback when `servers` isn't enough.\n\
- One OPERATION per document (the validator falsely flags disjoint variable sets across \
  operations). Multiple ALIASED FIELDS in one operation is fine and encouraged.\n\
- Drift signals: `notifications/tools/list_changed` / `notifications/prompts/list_changed` — \
  re-run discovery steps 0–1 if you receive one.\n\
\n\
Args: `query` (required GraphQL document), `variables` (optional JSON object), \
`operation_name` (optional). Returns the standard GraphQL response \
`{ \"data\": ..., \"errors\": ... }`."
    )]
    async fn query_graphql(
        &self,
        Parameters(args): Parameters<QueryGraphqlArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = vmcp_graphql::validation::pre_validate(&args.query) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "validation: {e}"
            ))]));
        }

        let schema_guard = self.inner.schema.load_full();
        let mut req = Request::new(args.query);
        if let Some(vars) = args.variables {
            req = req.variables(async_graphql::Variables::from_json(vars));
        }
        if let Some(op) = args.operation_name {
            req = req.operation_name(op);
        }
        let resp = schema_guard.execute(req).await;
        let body = serde_json::to_value(&resp).unwrap_or_else(|e| {
            json!({"errors": [{"message": format!("serialize: {e}")}]})
        });
        Ok(CallToolResult::success(vec![Content::text(body.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for VmcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut impl_info = Implementation::from_build_env();
        impl_info.name = "vmcp".into();
        impl_info.version = env!("CARGO_PKG_VERSION").into();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(impl_info)
            .with_instructions(
                "vmcp: Virtual MCP gateway. The ONLY tool is `query_graphql` — every upstream \
                 (postgres, time, jira, …) is reached through one typed GraphQL semantic \
                 layer, NOT through per-upstream tools.\n\
                 \n\
                 IMPORTANT — call `query_graphql` ONCE per user turn whenever possible. Pack \
                 every independent question the user asked as a separate ALIASED FIELD in one \
                 document; the server runs aliased fields in parallel. Calling `query_graphql` \
                 multiple times in a row for related queries is the anti-pattern — it wastes \
                 round-trips and tokens.\n\
                 \n\
                 Lazy discovery (only when field names are unknown): MCP `prompts/list` → \
                 `{ servers { ... } }` → `{ search(q) { ... } }` → narrow `__type(name)` → \
                 compose one aliased query/mutation. Avoid deep `__schema` introspection.",
            )
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        // Bind the ArcSwap snapshot to a local; iterate and build all the
        // owned Prompt structs before any `.await`. The Guard does not need
        // to live across an await point because this fn doesn't await.
        let skills = self.inner.skills.load();
        let prompts: Vec<Prompt> = skills
            .iter()
            .map(|s| {
                let args = if s.arguments.is_empty() {
                    None
                } else {
                    Some(
                        s.arguments
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
                Prompt::new(s.name.clone(), Some(s.description.clone()), args)
            })
            .collect();

        Ok(ListPromptsResult {
            meta: None,
            next_cursor: None,
            prompts,
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        // Take an owned Skill clone out of the snapshot, then drop the guard
        // before we touch anything that could await. (render_skill is sync
        // today, but cloning the Skill out also keeps Inner.skills hot-swap-
        // friendly: a parallel admin write can replace the Arc immediately.)
        let skill = {
            let skills = self.inner.skills.load();
            let found = skills
                .iter()
                .find(|s| s.name == request.name)
                .cloned()
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!("unknown prompt: {}", request.name),
                        None,
                    )
                })?;
            found
        };

        // rmcp gives us arguments as Option<JsonObject> = Option<Map<String, Value>>.
        // Convert to Handlebars-friendly HashMap<String, String> — non-string args
        // are JSON-stringified so templates can still interpolate them as text.
        let args: HashMap<String, String> = request
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| match v {
                Value::String(s) => (k, s),
                other => (k, other.to_string()),
            })
            .collect();

        let rendered = render_skill(&skill, &args).map_err(|e| {
            McpError::invalid_params(format!("render skill `{}`: {e}", skill.name), None)
        })?;

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            rendered,
        )])
        .with_description(skill.description.clone()))
    }
}
