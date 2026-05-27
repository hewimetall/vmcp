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
    CallToolRequestParam, CallToolResult, ClientCapabilities, ClientInfo, Implementation,
    LoggingMessageNotificationParam, ProgressNotificationParam, Tool,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
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
            .map(|kv| (kv.key().clone(), kv.value().resolved.load().as_ref().clone()))
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
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
    ) -> Result<CallToolResult> {
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
        let req = CallToolRequestParam::new(tool.to_string());
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
}

/// Spawn a single upstream. Public so tests can do one-shot spawns.
pub async fn spawn_one(
    spec: UpstreamSpec,
    bus: Arc<Bus>,
    spec_dir: Option<&std::path::Path>,
) -> Result<UpstreamSession> {
    debug!(name = %spec.name, command = %spec.command, args = ?spec.args, "spawning upstream");

    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args).envs(&spec.env);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.kill_on_drop(true);

    let transport = TokioChildProcess::new(cmd).context("spawn child process")?;

    let handler = ForwardingClient::new(spec.name.clone(), bus.clone());
    let client = handler
        .serve(transport)
        .await
        .context("MCP handshake with upstream")?;

    let live_tools = client.list_all_tools().await.context("upstream tools/list")?;

    let sidecar = resolve_sidecar(&spec, spec_dir)?;
    let cached: Vec<CachedTool> = live_tools
        .iter()
        .map(|t| CachedTool {
            name: t.name.to_string(),
            description: t.description.as_ref().map(|s| s.to_string()),
            input_schema: serde_json::to_value(&t.input_schema)
                .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
            read_only: tool_read_only_hint(t),
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
        })
        .collect();

    Ok(UpstreamSession {
        spec,
        client: Mutex::new(Some(client)),
        tools: ArcSwap::from_pointee(live_tools),
        resolved: ArcSwap::from_pointee(resolved),
        call_lock: Mutex::new(()),
        connected: AtomicBool::new(true),
    })
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
