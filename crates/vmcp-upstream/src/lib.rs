//! Upstream pool: spawns N stdio MCP servers as child processes and routes
//! tool calls to them.
//!
//! Replaces Python `vmcp/session_manager.py`. Parallel fan-out at boot via
//! `futures::future::join_all`; one bad upstream does not abort gateway
//! startup. Server-initiated notifications are forwarded to a
//! [`vmcp_notify::Bus`] for the rest of vmcp to subscribe to.

#![allow(clippy::result_large_err)]

mod sql_guard;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use futures::future::join_all;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, GetPromptRequestParams,
    GetPromptResult, Implementation, LoggingMessageNotificationParam, ProgressNotificationParam,
    Prompt, Tool,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use vmcp_notify::Bus;
use vmcp_registry::{
    apply_sidecar, load_sidecar, CachedTool, LockEntry, Registry, SidecarSpec, ToolsLock,
    UpstreamSpec,
};

/// Description of a single tool ready to be wired into GraphQL.
#[derive(Debug, Clone)]
pub struct ResolvedTool {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub read_only: bool,
    /// Sidecar-/upstream-merged task support. Non-forbidden → `run_task` allowlist.
    pub task_support: vmcp_registry::TaskSupportHint,
}

/// One upstream MCP prompt after `prompts/list` (arguments preserved as-is).
#[derive(Debug, Clone)]
pub struct ResolvedPrompt {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<ResolvedPromptArg>,
}

/// One prompt argument from upstream `prompts/list`.
#[derive(Debug, Clone)]
pub struct ResolvedPromptArg {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// A spawn failure that did NOT abort gateway boot.
#[derive(Debug)]
pub struct SpawnFailure {
    pub name: String,
    pub error: anyhow::Error,
}

/// Pool of running upstream sessions.
pub struct UpstreamPool {
    sessions: DashMap<String, Arc<UpstreamSession>>,
    bus: Arc<Bus>,
    call_timeout: Duration,
    /// Test-only stubs for [`Self::get_prompt`], keyed by `{server}\0{name}`.
    /// When present, the live client is skipped so unit tests can exercise
    /// GraphQL / proxy prompt get paths without spawning an MCP child.
    prompt_get_stubs: DashMap<String, GetPromptResult>,
}

/// One live stdio upstream.
pub struct UpstreamSession {
    pub spec: UpstreamSpec,
    /// Owned client handle. Dropped on shutdown.
    pub client: Mutex<Option<RunningService<RoleClient, ForwardingClient>>>,
    /// Raw rmcp Tool list (mostly diagnostic — we mostly read from `resolved`).
    pub tools: ArcSwap<Vec<Tool>>,
    /// Tools after sidecar overrides. The GraphQL builder reads this.
    pub resolved: ArcSwap<Vec<ResolvedTool>>,
    /// Upstream MCP prompts from `prompts/list`. Empty when the upstream has
    /// no prompts capability (spawn still succeeds).
    pub prompts: ArcSwap<Vec<ResolvedPrompt>>,
    /// Per-session call mutex (defence-in-depth, rmcp already serialises).
    pub call_lock: Mutex<()>,
    pub connected: AtomicBool,
}

impl UpstreamPool {
    /// Spawn every enabled upstream in `reg` in parallel. Failures are
    /// collected, never propagated — partial pools are normal.
    pub async fn spawn_all(
        reg: &Registry,
        bus: Arc<Bus>,
        spec_dir: Option<&std::path::Path>,
        spawn_timeout: Duration,
        call_timeout: Duration,
    ) -> (Self, Vec<SpawnFailure>) {
        let pool = Self {
            sessions: DashMap::new(),
            bus: bus.clone(),
            call_timeout,
            prompt_get_stubs: DashMap::new(),
        };

        let mut tasks = Vec::new();
        for spec in reg.upstreams.iter().filter(|s| s.enabled) {
            let bus = bus.clone();
            let spec = spec.clone();
            let spec_dir = spec_dir.map(|p| p.to_path_buf());
            tasks.push(async move {
                let name = spec.name.clone();
                let res = tokio::time::timeout(
                    spawn_timeout,
                    spawn_one(spec, bus.clone(), spec_dir.as_deref()),
                )
                .await;
                match res {
                    Ok(Ok(sess)) => Ok((name, sess)),
                    Ok(Err(e)) => Err(SpawnFailure { name, error: e }),
                    Err(_) => Err(SpawnFailure {
                        name,
                        error: anyhow!("spawn timed out"),
                    }),
                }
            });
        }

        let results = join_all(tasks).await;
        let mut failures = Vec::new();
        for r in results {
            match r {
                Ok((name, sess)) => {
                    pool.sessions.insert(name.clone(), Arc::new(sess));
                    info!(upstream = %name, "upstream session spawned");
                }
                Err(f) => {
                    error!(upstream = %f.name, error = %f.error, "upstream spawn failed");
                    failures.push(f);
                }
            }
        }

        (pool, failures)
    }

    /// Names of currently-connected upstreams.
    pub fn names(&self) -> Vec<String> {
        self.sessions.iter().map(|kv| kv.key().clone()).collect()
    }

    /// Empty pool for unit/integration tests (no live clients).
    pub fn empty_for_test(bus: Arc<Bus>) -> Self {
        Self {
            sessions: DashMap::new(),
            bus,
            call_timeout: Duration::from_secs(5),
            prompt_get_stubs: DashMap::new(),
        }
    }

    /// Install a canned `prompts/get` response for `(server, name)` used by
    /// unit tests (no live MCP client required).
    pub fn stub_prompt_get_for_test(&self, server: &str, name: &str, result: GetPromptResult) {
        self.prompt_get_stubs
            .insert(format!("{server}\0{name}"), result);
    }

    /// Register a synthetic upstream with pre-resolved tools (no live client).
    /// Intended for admin/API tests that need a non-empty pool snapshot.
    pub fn insert_synthetic_for_test(
        &self,
        name: impl Into<String>,
        description: Option<String>,
        tools: Vec<ResolvedTool>,
    ) {
        let name = name.into();
        let spec = UpstreamSpec {
            name: name.clone(),
            description,
            transport: Default::default(),
            url: None,
            bearer: None,
            command: String::new(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
        };
        let sess = UpstreamSession {
            spec,
            client: Mutex::new(None),
            tools: ArcSwap::from_pointee(vec![]),
            resolved: ArcSwap::from_pointee(tools),
            prompts: ArcSwap::from_pointee(vec![]),
            call_lock: Mutex::new(()),
            connected: AtomicBool::new(true),
        };
        self.sessions.insert(name, Arc::new(sess));
    }

    /// Register a synthetic upstream that also exposes prompts (tests).
    pub fn insert_synthetic_prompts_for_test(
        &self,
        name: impl Into<String>,
        description: Option<String>,
        tools: Vec<ResolvedTool>,
        prompts: Vec<ResolvedPrompt>,
    ) {
        let name = name.into();
        let spec = UpstreamSpec {
            name: name.clone(),
            description,
            transport: Default::default(),
            url: None,
            bearer: None,
            command: String::new(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
        };
        let sess = UpstreamSession {
            spec,
            client: Mutex::new(None),
            tools: ArcSwap::from_pointee(vec![]),
            resolved: ArcSwap::from_pointee(tools),
            prompts: ArcSwap::from_pointee(prompts),
            call_lock: Mutex::new(()),
            connected: AtomicBool::new(true),
        };
        self.sessions.insert(name, Arc::new(sess));
    }

    /// Operator-authored description for `server`, lifted from the registry
    /// entry. None if the upstream is unknown or has no description set.
    /// Powers `Query.servers.description` for cheap agent-side filtering.
    pub fn description_of(&self, server: &str) -> Option<String> {
        self.sessions
            .get(server)
            .and_then(|s| s.spec.description.clone())
    }

    /// Resolved tools for an upstream, or None if unknown.
    pub fn resolved(&self, server: &str) -> Option<Vec<ResolvedTool>> {
        self.sessions
            .get(server)
            .map(|s| s.resolved.load().as_ref().clone())
    }

    /// All resolved tools, grouped by server. Stable order by server name.
    pub fn all_resolved(&self) -> Vec<(String, Vec<ResolvedTool>)> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .map(|kv| {
                (
                    kv.key().clone(),
                    kv.value().resolved.load().as_ref().clone(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Resolved prompts for one upstream, or None if unknown.
    pub fn prompts(&self, server: &str) -> Option<Vec<ResolvedPrompt>> {
        self.sessions
            .get(server)
            .map(|s| s.prompts.load().as_ref().clone())
    }

    /// All resolved prompts, grouped by server. Stable order by server name.
    pub fn all_prompts(&self) -> Vec<(String, Vec<ResolvedPrompt>)> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().prompts.load().as_ref().clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Build LockEntries from current pool state (for lock file persistence).
    pub fn snapshot_lock(&self) -> Vec<LockEntry> {
        let mut out = Vec::new();
        for kv in self.sessions.iter() {
            let server = kv.key().clone();
            let tools: Vec<CachedTool> = kv
                .value()
                .resolved
                .load()
                .iter()
                .map(|t| CachedTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.input_schema.clone(),
                    read_only: t.read_only,
                    task_support: t.task_support,
                })
                .collect();
            out.push(LockEntry {
                server,
                tools,
                resolved_overrides: vec![],
            });
        }
        out.sort_by(|a, b| a.server.cmp(&b.server));
        out
    }

    /// Call an upstream tool. Returns the rmcp `CallToolResult` or an error if
    /// the upstream is gone / disconnected / timed out.
    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<CallToolResult> {
        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();

        if !sess.connected.load(Ordering::Relaxed) {
            return Err(anyhow!("upstream '{server}' is disconnected"));
        }

        let _guard = sess.call_lock.lock().await;

        let args_obj = match args {
            Value::Null => None,
            Value::Object(m) => Some(m),
            other => {
                return Err(anyhow!(
                    "tool args must be a JSON object or null, got: {other}"
                ));
            }
        };

        if server == "postgres" && tool == "query" {
            if let Some(sql) = args_obj
                .as_ref()
                .and_then(|m| m.get("sql"))
                .and_then(|v| v.as_str())
            {
                if let Err(guard_err) = crate::sql_guard::inspect(sql) {
                    let msg = format!("blocked by vmcp SQL guard: {guard_err}");
                    let mut result = CallToolResult::default();
                    result.content = vec![rmcp::model::Content::text(msg)];
                    result.is_error = Some(true);
                    return Ok(result);
                }
            }
        }

        // rmcp 1.7 made CallToolRequestParams non-exhaustive (added _meta, task).
        // Build via the constructor + builder.
        let req = CallToolRequestParams::new(tool.to_string());
        let req = match args_obj {
            Some(args) => req.with_arguments(args),
            None => req,
        };

        let client_guard = sess.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("upstream '{server}' has no client"))?;

        let res = tokio::time::timeout(self.call_timeout, client.call_tool(req))
            .await
            .map_err(|_| anyhow!("upstream '{server}' tool '{tool}' call timed out"))?
            .with_context(|| format!("upstream '{server}' tool '{tool}' call failed"))?;
        Ok(res)
    }

    /// Fetch an upstream prompt via `prompts/get`. Arguments are forwarded
    /// as-is (MCP string map). Returns an error if the upstream is unknown /
    /// disconnected / timed out / has no live client.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<GetPromptResult> {
        let stub_key = format!("{server}\0{name}");
        if let Some(stub) = self.prompt_get_stubs.get(&stub_key) {
            let _ = arguments; // stubs ignore args; production path uses them
            return Ok(stub.clone());
        }

        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();

        if !sess.connected.load(Ordering::Relaxed) {
            return Err(anyhow!("upstream '{server}' is disconnected"));
        }

        let _guard = sess.call_lock.lock().await;

        let req = GetPromptRequestParams::new(name.to_string());
        let req = match arguments {
            Some(args) => req.with_arguments(args),
            None => req,
        };

        let client_guard = sess.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("upstream '{server}' has no client"))?;

        let res = tokio::time::timeout(self.call_timeout, client.get_prompt(req))
            .await
            .map_err(|_| anyhow!("upstream '{server}' prompt '{name}' get timed out"))?
            .with_context(|| format!("upstream '{server}' prompt '{name}' get failed"))?;
        Ok(res)
    }

    /// Re-fetch `prompts/list` for one upstream and swap the cached catalogue.
    /// Best-effort: failures leave the previous cache in place and return Err.
    pub async fn refresh_prompts(&self, server: &str) -> Result<()> {
        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();

        if !sess.connected.load(Ordering::Relaxed) {
            return Err(anyhow!("upstream '{server}' is disconnected"));
        }

        let _guard = sess.call_lock.lock().await;
        let client_guard = sess.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("upstream '{server}' has no client"))?;

        let live = tokio::time::timeout(self.call_timeout, client.list_all_prompts())
            .await
            .map_err(|_| anyhow!("upstream '{server}' prompts/list timed out"))?
            .with_context(|| format!("upstream '{server}' prompts/list failed"))?;

        let resolved = resolve_prompts(server, live);
        info!(
            upstream = %server,
            count = resolved.len(),
            "refreshed upstream prompts cache"
        );
        sess.prompts.store(Arc::new(resolved));
        Ok(())
    }

    /// Gracefully cancel all upstreams. Best-effort.
    pub async fn shutdown(&self) {
        for kv in self.sessions.iter() {
            let mut guard = kv.value().client.lock().await;
            if let Some(c) = guard.take() {
                if let Err(e) = c.cancel().await {
                    warn!(upstream = %kv.key(), error = %e, "upstream cancel failed");
                }
            }
            kv.value().connected.store(false, Ordering::Relaxed);
        }
    }

    /// Bus used by this pool (for callers wiring up subscribers).
    pub fn bus(&self) -> Arc<Bus> {
        self.bus.clone()
    }

    /// Snapshot of live sessions for operator status APIs.
    pub fn status_snapshot(&self) -> Vec<UpstreamStatus> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .map(|kv| {
                let s = kv.value();
                UpstreamStatus {
                    name: kv.key().clone(),
                    description: s.spec.description.clone(),
                    transport: format!("{:?}", s.spec.transport).to_ascii_lowercase(),
                    connected: s.connected.load(Ordering::Relaxed),
                    tool_count: s.resolved.load().len(),
                    prompt_count: s.prompts.load().len(),
                    last_error: None,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Current registry specs held by live sessions (for reconcile diffs).
    pub fn specs_snapshot(&self) -> Vec<UpstreamSpec> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .map(|kv| kv.value().spec.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Cancel and remove one upstream. Missing name is a no-op.
    pub async fn remove(&self, name: &str) {
        let Some((_, sess)) = self.sessions.remove(name) else {
            return;
        };
        let mut guard = sess.client.lock().await;
        if let Some(c) = guard.take() {
            if let Err(e) = c.cancel().await {
                warn!(upstream = %name, error = %e, "upstream cancel failed on remove");
            }
        }
        sess.connected.store(false, Ordering::Relaxed);
        info!(upstream = %name, "upstream removed from pool");
    }

    /// Insert or replace a live session (caller already spawned it).
    pub async fn upsert(&self, name: String, sess: UpstreamSession) {
        if self.sessions.contains_key(&name) {
            self.remove(&name).await;
        }
        self.sessions.insert(name.clone(), Arc::new(sess));
        info!(upstream = %name, "upstream upserted into pool");
    }

    /// Re-run `tools/list` + sidecar merge for one upstream (list_changed path).
    pub async fn refresh_tools(
        &self,
        server: &str,
        spec_dir: Option<&std::path::Path>,
    ) -> Result<()> {
        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();
        if !sess.connected.load(Ordering::Relaxed) {
            return Err(anyhow!("upstream '{server}' is disconnected"));
        }
        let _guard = sess.call_lock.lock().await;
        let client_guard = sess.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("upstream '{server}' has no client"))?;

        let live_tools = tokio::time::timeout(self.call_timeout, client.list_all_tools())
            .await
            .map_err(|_| anyhow!("upstream '{server}' tools/list timed out"))?
            .with_context(|| format!("upstream '{server}' tools/list failed"))?;

        let sidecar = resolve_sidecar(&sess.spec, spec_dir)?;
        let cached: Vec<CachedTool> = live_tools
            .iter()
            .map(|t| CachedTool {
                name: t.name.to_string(),
                description: t.description.as_ref().map(|s| s.to_string()),
                input_schema: serde_json::to_value(&t.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
                read_only: tool_read_only_hint(t),
                task_support: tool_task_support_hint(t),
            })
            .collect();
        let (merged, _audit) = apply_sidecar(cached, sidecar.as_ref());
        let resolved: Vec<ResolvedTool> = merged
            .into_iter()
            .map(|c| ResolvedTool {
                server: server.to_string(),
                name: c.name,
                description: c.description,
                input_schema: c.input_schema,
                read_only: c.read_only,
                task_support: c.task_support,
            })
            .collect();
        info!(
            upstream = %server,
            count = resolved.len(),
            "refreshed upstream tools cache"
        );
        sess.tools.store(Arc::new(live_tools));
        sess.resolved.store(Arc::new(resolved));
        Ok(())
    }

    /// Spawn timeout used for reconcile add/replace.
    pub fn call_timeout(&self) -> Duration {
        self.call_timeout
    }
}

/// Operator-facing upstream status row.
#[derive(Debug, Clone)]
pub struct UpstreamStatus {
    pub name: String,
    pub description: Option<String>,
    pub transport: String,
    pub connected: bool,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub last_error: Option<String>,
}

/// Compare fields that require a session replace when they change.
pub fn spec_requires_respawn(a: &UpstreamSpec, b: &UpstreamSpec) -> bool {
    a.transport != b.transport
        || a.url != b.url
        || a.bearer != b.bearer
        || a.command != b.command
        || a.args != b.args
        || a.env != b.env
        || a.cwd != b.cwd
        || a.sidecar_spec != b.sidecar_spec
        || a.enabled != b.enabled
}

/// Spawn a single upstream. Public so tests can do one-shot spawns.
pub async fn spawn_one(
    spec: UpstreamSpec,
    bus: Arc<Bus>,
    spec_dir: Option<&std::path::Path>,
) -> Result<UpstreamSession> {
    let handler = ForwardingClient::new(spec.name.clone(), bus.clone());

    // Transport branch: a remote Streamable-HTTP MCP server or a
    // spawned stdio child process. Both yield the same RunningService type, so
    // the rest of the pool is transport-agnostic.
    let client = match spec.transport {
        vmcp_registry::UpstreamTransport::Http => {
            let url = spec.url.clone().context("http upstream requires `url`")?;
            debug!(name = %spec.name, %url, "connecting http upstream");
            let mut config = StreamableHttpClientTransportConfig::with_uri(url);
            if let Some(token) = &spec.bearer {
                config = config.auth_header(token.clone());
            }
            let transport = StreamableHttpClientTransport::from_config(config);
            handler
                .serve(transport)
                .await
                .context("MCP handshake with http upstream")?
        }
        vmcp_registry::UpstreamTransport::Stdio => {
            debug!(name = %spec.name, command = %spec.command, args = ?spec.args, "spawning upstream");
            let mut cmd = Command::new(&spec.command);
            cmd.args(&spec.args).envs(&spec.env);
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            cmd.kill_on_drop(true);
            let transport = TokioChildProcess::new(cmd).context("spawn child process")?;
            handler
                .serve(transport)
                .await
                .context("MCP handshake with upstream")?
        }
    };

    let live_tools = client
        .list_all_tools()
        .await
        .context("upstream tools/list")?;

    // Prompts are optional — many upstreams lack the capability. Never fail
    // spawn when prompts/list is unsupported or empty.
    let live_prompts = match client.list_all_prompts().await {
        Ok(p) => p,
        Err(e) => {
            debug!(
                upstream = %spec.name,
                error = %e,
                "upstream prompts/list unavailable; continuing with empty prompts"
            );
            Vec::new()
        }
    };
    let resolved_prompts = resolve_prompts(&spec.name, live_prompts);

    let sidecar = resolve_sidecar(&spec, spec_dir)?;
    let cached: Vec<CachedTool> = live_tools
        .iter()
        .map(|t| CachedTool {
            name: t.name.to_string(),
            description: t.description.as_ref().map(|s| s.to_string()),
            input_schema: serde_json::to_value(&t.input_schema)
                .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
            read_only: tool_read_only_hint(t),
            task_support: tool_task_support_hint(t),
        })
        .collect();
    let (merged, _audit) = apply_sidecar(cached, sidecar.as_ref());

    let resolved: Vec<ResolvedTool> = merged
        .into_iter()
        .map(|c| ResolvedTool {
            server: spec.name.clone(),
            name: c.name,
            description: c.description,
            input_schema: c.input_schema,
            read_only: c.read_only,
            task_support: c.task_support,
        })
        .collect();

    Ok(UpstreamSession {
        spec,
        client: Mutex::new(Some(client)),
        tools: ArcSwap::from_pointee(live_tools),
        resolved: ArcSwap::from_pointee(resolved),
        prompts: ArcSwap::from_pointee(resolved_prompts),
        call_lock: Mutex::new(()),
        connected: AtomicBool::new(true),
    })
}

fn resolve_prompts(server: &str, prompts: Vec<Prompt>) -> Vec<ResolvedPrompt> {
    prompts
        .into_iter()
        .map(|p| ResolvedPrompt {
            server: server.to_string(),
            name: p.name,
            description: p.description,
            arguments: p
                .arguments
                .unwrap_or_default()
                .into_iter()
                .map(|a| ResolvedPromptArg {
                    name: a.name,
                    description: a.description,
                    required: a.required.unwrap_or(false),
                })
                .collect(),
        })
        .collect()
}

fn resolve_sidecar(
    spec: &UpstreamSpec,
    spec_dir: Option<&std::path::Path>,
) -> Result<Option<SidecarSpec>> {
    let path = match &spec.sidecar_spec {
        Some(p) if p.is_absolute() => Some(p.clone()),
        Some(p) => spec_dir.map(|d| d.join(p)).or(Some(p.clone())),
        None => None,
    };
    Ok(load_sidecar(path.as_deref())?)
}

/// Best-effort read of the MCP `readOnlyHint` tool annotation. Falls back to
/// `false` (mutation-bucket) if the annotation is absent — safer than
/// silently exposing a write-tool as read-only.
fn tool_read_only_hint(tool: &Tool) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false)
}

/// Map upstream `execution.taskSupport` into the registry hint used for the
/// `run_task` allowlist. Absent / forbidden → not a task tool.
fn tool_task_support_hint(tool: &Tool) -> vmcp_registry::TaskSupportHint {
    use rmcp::model::TaskSupport;
    use vmcp_registry::TaskSupportHint;
    match tool.task_support() {
        TaskSupport::Optional => TaskSupportHint::Optional,
        TaskSupport::Required => TaskSupportHint::Required,
        TaskSupport::Forbidden => TaskSupportHint::Forbidden,
    }
}

/// rmcp client handler that forwards every server-initiated notification onto
/// the in-process bus.
#[derive(Clone)]
pub struct ForwardingClient {
    source: String,
    bus: Arc<Bus>,
}

impl ForwardingClient {
    pub fn new(source: String, bus: Arc<Bus>) -> Self {
        Self { source, bus }
    }
}

impl ClientHandler for ForwardingClient {
    fn get_info(&self) -> ClientInfo {
        // rmcp 1.7 made InitializeRequestParams (alias ClientInfo) and
        // Implementation non-exhaustive — must use constructors.
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("vmcp", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _ctx: NotificationContext<RoleClient>) {
        self.bus.publish(
            self.source.clone(),
            "notifications/tools/list_changed",
            serde_json::json!({}),
        );
    }

    async fn on_prompt_list_changed(&self, _ctx: NotificationContext<RoleClient>) {
        self.bus.publish(
            self.source.clone(),
            "notifications/prompts/list_changed",
            serde_json::json!({}),
        );
    }

    async fn on_resource_list_changed(&self, _ctx: NotificationContext<RoleClient>) {
        self.bus.publish(
            self.source.clone(),
            "notifications/resources/list_changed",
            serde_json::json!({}),
        );
    }

    async fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        let value = serde_json::to_value(&params).unwrap_or(Value::Null);
        self.bus.publish(
            self.source.clone(),
            "notifications/resources/updated",
            value,
        );
    }

    async fn on_cancelled(
        &self,
        params: rmcp::model::CancelledNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        let value = serde_json::to_value(&params).unwrap_or(Value::Null);
        self.bus
            .publish(self.source.clone(), "notifications/cancelled", value);
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        let value = serde_json::to_value(&params).unwrap_or(Value::Null);
        self.bus
            .publish(self.source.clone(), "notifications/progress", value);
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        let value = serde_json::to_value(&params).unwrap_or(Value::Null);
        self.bus
            .publish(self.source.clone(), "notifications/message", value);
    }
}

/// Build a fresh ToolsLock from a pool snapshot. Helper for the bin crate.
pub fn build_lock_from_pool(pool: &UpstreamPool) -> ToolsLock {
    ToolsLock::new(pool.snapshot_lock())
}

/// Re-export so callers don't have to depend on vmcp-registry directly for
/// the common case of consuming this crate.
pub use vmcp_notify as notify;
pub use vmcp_registry as registry;
