//! The MCP server surface: `query_graphql` (sync aggregation) plus optional
//! `run_task` (SEP-1686) for upstream tools that advertise task support.
//!
//! Replaces Python `vmcp/mcp_app.py` + `tools/query_graphql.py`. Code Mode
//! (execute_code / get_code_result / Dagger sandbox) is intentionally not
//! reimplemented — see the presentation for the talk-level justification.
//!
//! The MCP surfaces this server exposes:
//!
//! * **Tools** — `query_graphql` always; `run_task` when `[tasks].enabled` and
//!   at least one upstream tool is task-capable. Discovery for GraphQL happens
//!   inside the schema (`servers`, `search`, `__type`).
//! * **Prompts** — operator-authored YAML files in `skills_dir` become MCP
//!   prompts via `prompts/list` and `prompts/get`. See [`skills`].

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_graphql::dynamic::Schema;
use async_graphql::Request;
use dashmap::DashMap;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::{NotificationContext, Peer, RequestContext};
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};

use vmcp_upstream::UpstreamPool;

pub use tasks::RUN_TASK_TOOL;

pub mod skills;
pub use skills::{delete_skill, load_skills, render_skill, save_skill, Skill, SkillArg};

#[cfg(feature = "otel")]
pub mod otel_file;
pub mod recorder;
pub mod sessions;

pub mod tasks;
pub use tasks::{collect_task_allowlist, TaskError, TaskRunner, TaskStore};

pub mod proxy;
pub use proxy::ProxyServer;

pub mod graphql_inject;
pub mod prompt_catalog;
pub mod prompt_proxy;
pub use prompt_catalog::prompt_source_handlers;

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
    /// Native MCP Tasks wiring. `Some` only when `[tasks].enabled` and the
    /// allowlist is non-empty — gates both `run_task` and the `tasks` capability.
    tasks: Option<RunTaskTool>,
    /// Connected client sessions, captured on `initialized`. The notification
    /// forwarder fans upstream MCP events out to these peers.
    peers: Arc<DashMap<u64, Peer<RoleServer>>>,
    /// Monotonic id generator for `peers` keys.
    peer_seq: Arc<AtomicU64>,
}

/// The gated `run_task` tool plus its SQLite-backed task runner.
struct RunTaskTool {
    runner: Arc<TaskRunner>,
    /// Advertised `Tool` (carries `execution.taskSupport = optional`).
    tool: Tool,
}

/// Build the advertised `run_task` tool: proxy to allowlisted upstream tools.
fn build_run_task_tool() -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "description": "Invoke a task-capable upstream tool. Only tools with execution.taskSupport (or sidecar task_support) are allowed.",
        "properties": {
            "server": {
                "type": "string",
                "description": "Upstream server name (GraphQL namespace / registry name)."
            },
            "tool": {
                "type": "string",
                "description": "Upstream tool name as advertised by that server."
            },
            "arguments": {
                "type": "object",
                "description": "Arguments forwarded verbatim to the upstream tool.",
                "additionalProperties": true
            }
        },
        "required": ["server", "tool"]
    });
    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    let mut tool = Tool::new_with_raw(
        RUN_TASK_TOOL,
        Some(
            "Run a task-capable upstream tool. Prefer augmenting tools/call with `task` \
             for long runs: a task is created immediately (SQLite-backed) and you poll \
             `tasks/get` / `tasks/result`. Calling it normally runs synchronously and \
             blocks for the result. Short / batched reads should still use `query_graphql`."
                .into(),
        ),
        Arc::new(schema_obj),
    );
    tool.execution = Some(ToolExecution::new().with_task_support(TaskSupport::Optional));
    tool
}

/// Owner key for task context-binding.
///
/// Phase 1 runs single-tenant: every requestor shares the `"anon"` bucket, so
/// task IDs (UUID) are the access-control mechanism. Per-requestor binding can
/// be layered on later by extracting an identity from `context`.
fn task_owner(_context: &RequestContext<RoleServer>) -> String {
    "anon".to_string()
}

/// Parse `run_task` arguments: `{ server, tool, arguments? }`.
fn parse_run_task_args(
    arguments: Option<serde_json::Map<String, Value>>,
) -> Result<(String, String, Value), McpError> {
    let args = arguments.unwrap_or_default();
    let server = args
        .get("server")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpError::invalid_params("run_task requires string `server`", None))?;
    let tool = args
        .get("tool")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpError::invalid_params("run_task requires string `tool`", None))?;
    let forwarded = args
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    Ok((server, tool, forwarded))
}

/// Map a [`TaskError`] to the JSON-RPC error the Tasks spec prescribes
/// (`-32602` Invalid params for missing / already-terminal tasks).
fn task_err_to_mcp(e: TaskError) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

/// Fan a single bus notification out to every connected client peer, mapping
/// the bus method string to the matching typed MCP notification. Peers whose
/// send fails (disconnected sessions) are pruned.
async fn forward_to_peers(peers: &DashMap<u64, Peer<RoleServer>>, n: &vmcp_notify::Notification) {
    // Snapshot peers before awaiting so we never hold a DashMap shard lock
    // across an `.await`.
    let snapshot: Vec<(u64, Peer<RoleServer>)> = peers
        .iter()
        .map(|kv| (*kv.key(), kv.value().clone()))
        .collect();
    if snapshot.is_empty() {
        return;
    }
    let mut dead = Vec::new();
    for (id, peer) in snapshot {
        let res = send_one(&peer, &n.method, &n.params).await;
        if res.is_err() {
            dead.push(id);
        }
    }
    for id in dead {
        peers.remove(&id);
    }
}

/// Translate one bus notification into the right server→client notification.
/// Unknown methods are ignored (per JSON-RPC, receivers must ignore unknown
/// notifications anyway).
async fn send_one(
    peer: &Peer<RoleServer>,
    method: &str,
    params: &Value,
) -> Result<(), rmcp::service::ServiceError> {
    match method {
        "notifications/tools/list_changed" => peer.notify_tool_list_changed().await,
        "notifications/prompts/list_changed" => peer.notify_prompt_list_changed().await,
        "notifications/resources/list_changed" => peer.notify_resource_list_changed().await,
        "notifications/resources/updated" => match serde_json::from_value(params.clone()) {
            Ok(p) => peer.notify_resource_updated(p).await,
            Err(_) => Ok(()),
        },
        "notifications/progress" => match serde_json::from_value(params.clone()) {
            Ok(p) => peer.notify_progress(p).await,
            Err(_) => Ok(()),
        },
        "notifications/message" => match serde_json::from_value(params.clone()) {
            Ok(p) => peer.notify_logging_message(p).await,
            Err(_) => Ok(()),
        },
        "notifications/cancelled" => match serde_json::from_value(params.clone()) {
            Ok(p) => peer.notify_cancelled(p).await,
            Err(_) => Ok(()),
        },
        _ => Ok(()),
    }
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
        Self::with_tasks(schema, pool, skills, None)
    }

    /// Like [`new`](Self::new) but wires the native-task `run_task` runner.
    /// Pass `Some(runner)` only when `[tasks].enabled` and the allowlist is
    /// non-empty — it registers `run_task` and turns on the server `tasks`
    /// capability.
    pub fn with_tasks(
        schema: SchemaHandle,
        pool: Arc<UpstreamPool>,
        skills: SkillsHandle,
        tasks: Option<Arc<TaskRunner>>,
    ) -> Self {
        let tasks = tasks.map(|runner| RunTaskTool {
            runner,
            tool: build_run_task_tool(),
        });
        Self {
            inner: Arc::new(Inner {
                schema,
                pool,
                skills,
                tasks,
                peers: Arc::new(DashMap::new()),
                peer_seq: Arc::new(AtomicU64::new(0)),
            }),
            tool_router: Self::tool_router(),
        }
    }

    /// Subscribe to the upstream notification bus and fan every server-initiated
    /// event from internal MCP servers out to connected clients as the matching
    /// MCP `notifications/*`. Spawns a detached task; call once after boot.
    ///
    /// This is the push side of "vmcp forwards events from internal MCPs". Pull
    /// remains available via `query_graphql { notifications }`. Clients only act
    /// on notifications they understand (e.g. `tools/list_changed`); others are
    /// delivered best-effort.
    pub fn spawn_notification_forwarder(&self) {
        let peers = self.inner.peers.clone();
        let bus = self.inner.pool.bus();
        let pool = self.inner.pool.clone();
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(n) => {
                        // Refresh the prompt cache *before* forwarding so a
                        // client that re-lists / getPrompt immediately after
                        // the notification sees the post-drift catalogue.
                        if n.method == "notifications/prompts/list_changed" {
                            if let Err(e) = pool.refresh_prompts(&n.source).await {
                                tracing::warn!(
                                    upstream = %n.source,
                                    error = %e,
                                    "failed to refresh prompts after list_changed"
                                );
                            }
                        }
                        forward_to_peers(&peers, &n).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
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
        description = r#"Execute GraphQL against the vmcp semantic layer. This is the ONLY tool — every upstream MCP server (postgres, time, jira, etc.) is reached through ONE typed GraphQL surface, NOT through separate per-upstream tools.

═══════════════════════════════════════════════════════════════
RULE #1 — BATCH EVERYTHING INTO ONE CALL.
═══════════════════════════════════════════════════════════════

GraphQL aliased fields execute in PARALLEL inside a single document. The whole point of this tool is that you write ONE document covering EVERY piece of data the user asked for, and the server fans out concurrently. Do not call `query_graphql` multiple times in a row for related questions — that is the single biggest waste of tokens and round-trips you can do here.

❌ ANTI-PATTERN (do NOT do this):
turn 1: query_graphql({ time { getCurrentTime(timezone: "Europe/Moscow") { json } } })
turn 2: query_graphql({ time { getCurrentTime(timezone: "Asia/Tokyo") { json } } })
turn 3: query_graphql({ postgres { query(sql: "SELECT ...") { json } } })

✓ CORRECT (one call, three aliased fields run concurrently):
query_graphql({
moscow:    time { getCurrentTime(timezone: "Europe/Moscow") { json } }
tokyo:     time { getCurrentTime(timezone: "Asia/Tokyo")    { json } }
customers: postgres { query(sql: "SELECT name, country FROM customers") { json } }
})

Multiple top-level aliases in one document = one network round-trip = one entry in the audit log. The user's question may MENTION several things — count them, alias them all in ONE document, send it once. If you find yourself thinking "first I'll fetch X, then Y" — stop, combine them.

Aliasing inside a single SQL: use SQL features (UNION ALL, JOIN, CASE) to pack multiple related questions into one `postgres.query` field. Even cheaper than two GraphQL aliases.

═══════════════════════════════════════════════════════════════
RULE #1B — BREAKDOWN + TOTALS = ONE CALL, ALWAYS TWO ALIASES.
═══════════════════════════════════════════════════════════════

When the user asks for a grouped breakdown AND aggregate totals over the same data ("count per X, plus the total"), it is ONE call with TWO aliased postgres queries — never a breakdown call followed by a totals call.

❌ ANTI-PATTERN (do NOT do this — wastes a round-trip):
turn 1: query_graphql({ postgres { query(sql: "SELECT country, COUNT(*) FROM customers GROUP BY country") { json } } })
turn 2: query_graphql({ postgres { query(sql: "SELECT COUNT(*) total, COUNT(DISTINCT country) countries FROM customers") { json } } })

✓ CORRECT (one call, both aliases at the postgres level):
query_graphql({
postgres {
by_country: query(sql: "SELECT country, COUNT(*) AS n FROM customers GROUP BY country") { json }
totals:     query(sql: "SELECT COUNT(*) AS total, COUNT(DISTINCT country) AS countries FROM customers") { json }
}
})

Re-read the user message before you write the query. Count the distinct sub-questions ("breakdown", "total", "top-N", "per X", etc.) — your document must have at LEAST that many alias fields. Two sub-questions = at least two aliases, not two calls.

═══════════════════════════════════════════════════════════════
RULE #1C — PER-CATEGORY LOOPS ARE A BUG. USE GROUP BY.
═══════════════════════════════════════════════════════════════

If you find yourself about to issue one query per category ("SELECT … WHERE country='RU'", then "… country='DE'", …), STOP. That's the same per-row anti-pattern in disguise.

❌ ANTI-PATTERN:
turn 1: query(sql: "SELECT COUNT(*) FROM customers WHERE country='RU'")
turn 2: query(sql: "SELECT COUNT(*) FROM customers WHERE country='DE'")
turn 3: query(sql: "SELECT COUNT(*) FROM customers WHERE country='JP'")

✓ CORRECT — one SQL, GROUP BY does the per-category split for you:
query(sql: "SELECT country, COUNT(*) FROM customers GROUP BY country")

═══════════════════════════════════════════════════════════════
DISCOVERY LADDER (only when you don't already know the field names)
═══════════════════════════════════════════════════════════════

Start at the cheapest step. Skip steps you don't need. Then write ONE batched call.

0. Skill discovery — GraphQL `{ prompts { name description source arguments { name required } } }` then `{ getPrompt(name: "…", arguments: {…}) { text } }` (preferred mid-flight), or MCP `prompts/list` + `prompts/get`. Local YAML skills use bare names on `/mcp`; upstream prompts (`{server}__{name}`) appear in GraphQL and on `/mcp-proxy` when `[proxy]` is enabled. Follow the rendered body VERBATIM (upstream bodies start with a GraphQL tool-routing table).
1. `{ servers { name description toolCount readOnlyCount } }` — catalogue of upstreams. Cheapest GraphQL probe. Combine with the real query in one document if you want both.
2. `{ search(q: "<keywords>") { server tool readOnly description } }` / `{ searchPrompts(q: "…") { name description source } }` — token-level full-text match across tools or prompts, ranked. Token match is case-insensitive, no fuzzy/synonyms — pick descriptive keywords.
3. `{ ns: __type(name: "<Pascal(server)><Read|Write>") { fields { name description args { name description type { kind name ofType { kind name ofType { kind name }}}}}}}` — typed signature for ONE namespace. Name is `PascalCase(server)` + `Read` if `readOnly` is true else `Write`.
4. Compose the real call as ONE aliased document.

═══════════════════════════════════════════════════════════════
SHAPE & RULES
═══════════════════════════════════════════════════════════════

Query    `{ <serverCamel> { <toolCamel>(args) { json text isError } } }`
Mutation `mutation { <serverCamel> { <toolCamel>(args) { json text isError } } }`
Read vs Write split: by upstream's `readOnlyHint`. search/list/get → Query.foo, create/update/delete → Mutation.foo.

- DO NOT request `__schema { types { ... fields { ... } ... } }` with deep nesting — dumps the entire catalogue and burns context. Shallow `__schema { types { name kind } }` ok as a fallback when `servers` isn't enough.
- One OPERATION per document (the validator falsely flags disjoint variable sets across operations). Multiple ALIASED FIELDS in one operation is fine and encouraged.
- Drift signals: `notifications/tools/list_changed` / `notifications/prompts/list_changed` — re-run discovery steps 0–1 if you receive one.

Args: `query` (required GraphQL document), `variables` (optional JSON object), `operation_name` (optional). Returns the standard GraphQL response `{ "data": ..., "errors": ... }`."#
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
        let body = serde_json::to_value(&resp)
            .unwrap_or_else(|e| json!({"errors": [{"message": format!("serialize: {e}")}]}));
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }
}

impl VmcpServer {
    /// The task store, or a JSON-RPC error when tasks are not enabled.
    fn task_store(&self) -> Result<Arc<TaskStore>, McpError> {
        self.inner
            .tasks
            .as_ref()
            .map(|d| d.runner.store())
            .ok_or_else(McpError::method_not_found::<GetTaskInfoMethod>)
    }
}

#[tool_handler]
impl ServerHandler for VmcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut impl_info = Implementation::from_build_env();
        impl_info.name = "vmcp".into();
        impl_info.version = env!("CARGO_PKG_VERSION").into();
        let mut caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_prompts()
            .enable_prompts_list_changed()
            .build();
        // Native MCP Tasks (SEP-1686) — only when `run_task` is wired.
        if self.inner.tasks.is_some() {
            caps.tasks = Some(TasksCapability::server_default());
        }
        let instructions: String = if self.inner.tasks.is_some() {
            "vmcp: Virtual MCP gateway.\n\
             \n\
             Tools:\n\
             - `query_graphql` — sync typed aggregation over upstreams (preferred for \
               short reads / batches). The gateway awaits upstreams and returns GraphQL JSON.\n\
             - `run_task` — SEP-1686 task proxy for upstream tools with `taskSupport` \
               (`optional`/`required` in `search`). Augment with `task` for async \
               (CreateTaskResult → tasks/get / tasks/result); call without `task` for sync.\n\
             \n\
             IMPORTANT — call `query_graphql` ONCE per user turn whenever possible. Pack \
             every independent question as a separate ALIASED FIELD in one document.\n\
             \n\
             Lazy discovery: `{ prompts { name description source } }` / \
             `getPrompt(name)` → `{ servers { ... } }` → \
             `{ search(q) { server tool readOnly taskSupport description } }` → \
             `__type(name)` → compose one aliased query/mutation."
                .to_string()
        } else {
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
             Lazy discovery (only when field names are unknown): `{ prompts { ... } }` / \
             `getPrompt` → `{ servers { ... } }` → `{ search(q) { ... } }` → narrow \
             `__type(name)` → compose one aliased query/mutation. Avoid deep `__schema` \
             introspection."
                .to_string()
        };
        ServerInfo::new(caps)
            .with_server_info(impl_info)
            .with_instructions(instructions)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let id = self.inner.peer_seq.fetch_add(1, Ordering::Relaxed);
        self.inner.peers.insert(id, context.peer.clone());
    }

    // ---- Tools: merge the static `query_graphql` router with gated `run_task` ----

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        if let Some(t) = &self.inner.tasks {
            tools.push(t.tool.clone());
        }
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == RUN_TASK_TOOL {
            return self.inner.tasks.as_ref().map(|d| d.tool.clone());
        }
        self.tool_router.get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // `run_task` invoked normally (non-task path of an `optional` tool):
        // run synchronously by proxying to the allowlisted upstream tool.
        if request.name == RUN_TASK_TOOL {
            let Some(t) = &self.inner.tasks else {
                return Err(McpError::invalid_params("run_task is not enabled", None));
            };
            let (server, tool, args) = parse_run_task_args(request.arguments)?;
            return match t.runner.run_now(&server, &tool, args).await {
                Ok(r) => Ok(r),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!("{e:#}"))])),
            };
        }
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    // ---- Native MCP Tasks (SEP-1686): only `run_task` is augmentable ----

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        let Some(t) = &self.inner.tasks else {
            return Err(McpError::invalid_params(
                "task-based invocation is not supported",
                None,
            ));
        };
        if request.name != RUN_TASK_TOOL {
            return Err(McpError::invalid_params(
                format!(
                    "tool '{}' does not support task-based invocation",
                    request.name
                ),
                None,
            ));
        }
        let (server, tool, args) = parse_run_task_args(request.arguments)?;
        let owner = task_owner(&context);
        t.runner
            .enqueue(owner, server, tool, args)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let store = self.task_store()?;
        store
            .get(&request.task_id, &task_owner(&context))
            .map_err(task_err_to_mcp)
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let store = self.task_store()?;
        let payload = store
            .await_result(&request.task_id, &task_owner(&context))
            .await
            .map_err(task_err_to_mcp)?;
        Ok(GetTaskPayloadResult::new(payload))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        let store = self.task_store()?;
        store
            .cancel(&request.task_id, &task_owner(&context))
            .map_err(task_err_to_mcp)
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        let store = self.task_store()?;
        Ok(store.list(&task_owner(&context)))
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
                    McpError::invalid_params(format!("unknown prompt: {}", request.name), None)
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
