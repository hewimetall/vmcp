//! Upstream pool: spawns N stdio MCP servers as child processes and routes
//! tool calls to them.
//!
//! Replaces Python `vmcp/session_manager.py`. Parallel fan-out at boot via
//! `futures::future::join_all`; one bad upstream does not abort gateway
//! startup. Server-initiated notifications are forwarded to a
//! [`vmcp_notify::Bus`] for the rest of vmcp to subscribe to.

#![allow(clippy::result_large_err)]

mod identity;
mod sql_guard;

pub use identity::{
    CallerIdentity, CallerSlot, HEADER_CLIENT_ID, HEADER_GROUPS, HEADER_SCOPE, HEADER_SUBJECT,
};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use futures::future::join_all;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
    GetPromptRequestParams, GetPromptResult, Implementation, ProgressNotificationParam, Prompt,
    Tool,
};
// SEP-2577 deprecated MCP logging; keep forwarding upstream messages until
// clients migrate off notifications/message.
#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
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
    /// Hot-swappable registry entry (description can change without respawn).
    pub spec: ArcSwap<UpstreamSpec>,
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
    /// Per-call caller identity for HTTP `X-Vmcp-*` injection (shared with
    /// [`identity::IdentityHttpClient`]).
    pub caller_slot: identity::CallerSlot,
    /// Last observed liveness: updated on successful/failed RPC. Advisory for
    /// `/api/v1/upstreams` + `/ready` — call paths still retry while a client
    /// handle exists.
    pub connected: AtomicBool,
    /// Last transport/RPC error message (cleared on success).
    pub last_error: ArcSwap<Option<String>>,
    /// Unix ms of last successful RPC (0 = never).
    pub last_ok_unix_ms: AtomicU64,
}

impl UpstreamSession {
    fn mark_ok(&self) {
        self.connected.store(true, Ordering::Relaxed);
        self.last_error.store(Arc::new(None));
        self.last_ok_unix_ms.store(unix_ms_now(), Ordering::Relaxed);
    }

    fn mark_err(&self, err: impl ToString) {
        self.connected.store(false, Ordering::Relaxed);
        self.last_error.store(Arc::new(Some(err.to_string())));
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_session_fields(connected: bool) -> (AtomicBool, ArcSwap<Option<String>>, AtomicU64) {
    (
        AtomicBool::new(connected),
        ArcSwap::from_pointee(None),
        AtomicU64::new(if connected { unix_ms_now() } else { 0 }),
    )
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
            forward_identity: false,
        };
        let (connected, last_error, last_ok_unix_ms) = new_session_fields(true);
        let sess = UpstreamSession {
            spec: ArcSwap::from_pointee(spec),
            client: Mutex::new(None),
            tools: ArcSwap::from_pointee(vec![]),
            resolved: ArcSwap::from_pointee(tools),
            prompts: ArcSwap::from_pointee(vec![]),
            call_lock: Mutex::new(()),
            caller_slot: identity::new_caller_slot(),
            connected,
            last_error,
            last_ok_unix_ms,
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
            forward_identity: false,
        };
        let (connected, last_error, last_ok_unix_ms) = new_session_fields(true);
        let sess = UpstreamSession {
            spec: ArcSwap::from_pointee(spec),
            client: Mutex::new(None),
            tools: ArcSwap::from_pointee(vec![]),
            resolved: ArcSwap::from_pointee(tools),
            prompts: ArcSwap::from_pointee(prompts),
            call_lock: Mutex::new(()),
            caller_slot: identity::new_caller_slot(),
            connected,
            last_error,
            last_ok_unix_ms,
        };
        self.sessions.insert(name, Arc::new(sess));
    }

    /// Operator-authored description for `server`, lifted from the registry
    /// entry. None if the upstream is unknown or has no description set.
    /// Powers `Query.servers.description` for cheap agent-side filtering.
    pub fn description_of(&self, server: &str) -> Option<String> {
        self.sessions
            .get(server)
            .and_then(|s| s.spec.load().description.clone())
    }

    /// Update description without respawning (G15).
    pub fn set_description(&self, server: &str, description: Option<String>) -> bool {
        let Some(sess) = self.sessions.get(server) else {
            return false;
        };
        let mut next = (**sess.spec.load()).clone();
        next.description = description;
        sess.spec.store(Arc::new(next));
        true
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
    /// the upstream is gone / timed out. Updates per-session status on outcome.
    ///
    /// When `caller` is set and the upstream has `forward_identity = true`
    /// (opt-in for internal adapters), HTTP transports attach
    /// `X-Vmcp-Subject` / `X-Vmcp-Groups` / … for the duration of this call
    /// (serialized by `call_lock`). Registry `bearer` remains the service
    /// `Authorization` credential. External SaaS keep the default `false`.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        caller: Option<&CallerIdentity>,
    ) -> Result<CallToolResult> {
        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();

        let _guard = sess.call_lock.lock().await;

        let forward = sess.spec.load().forward_identity;
        if forward {
            if let Some(c) = caller {
                sess.caller_slot.store(Arc::new(Some(c.clone())));
            }
        }
        let result = self.call_locked(&sess, server, tool, args).await;
        if forward {
            sess.caller_slot.store(Arc::new(None));
        }
        result
    }

    async fn call_locked(
        &self,
        sess: &UpstreamSession,
        server: &str,
        tool: &str,
        args: Value,
    ) -> Result<CallToolResult> {
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
                    result.content = vec![ContentBlock::text(msg)];
                    result.is_error = Some(true);
                    // SQL guard is a local policy reject — not an upstream outage.
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
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => {
                let msg = format!("upstream '{server}' has no client");
                sess.mark_err(&msg);
                return Err(anyhow!(msg));
            }
        };

        let res = tokio::time::timeout(self.call_timeout, client.call_tool(req)).await;
        match res {
            Ok(Ok(r)) => {
                sess.mark_ok();
                Ok(r)
            }
            Ok(Err(e)) => {
                let err =
                    anyhow!(e).context(format!("upstream '{server}' tool '{tool}' call failed"));
                sess.mark_err(format!("{err:#}"));
                Err(err)
            }
            Err(_) => {
                let msg = format!("upstream '{server}' tool '{tool}' call timed out");
                sess.mark_err(&msg);
                Err(anyhow!(msg))
            }
        }
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

        let _guard = sess.call_lock.lock().await;

        let req = GetPromptRequestParams::new(name.to_string());
        let req = match arguments {
            Some(args) => req.with_arguments(args),
            None => req,
        };

        let client_guard = sess.client.lock().await;
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => {
                let msg = format!("upstream '{server}' has no client");
                sess.mark_err(&msg);
                return Err(anyhow!(msg));
            }
        };

        match tokio::time::timeout(self.call_timeout, client.get_prompt(req)).await {
            Ok(Ok(r)) => {
                sess.mark_ok();
                Ok(r)
            }
            Ok(Err(e)) => {
                let err =
                    anyhow!(e).context(format!("upstream '{server}' prompt '{name}' get failed"));
                sess.mark_err(format!("{err:#}"));
                Err(err)
            }
            Err(_) => {
                let msg = format!("upstream '{server}' prompt '{name}' get timed out");
                sess.mark_err(&msg);
                Err(anyhow!(msg))
            }
        }
    }

    /// Re-fetch `prompts/list` for one upstream and swap the cached catalogue.
    /// Best-effort: failures leave the previous cache in place and return Err.
    pub async fn refresh_prompts(&self, server: &str) -> Result<()> {
        let sess = self
            .sessions
            .get(server)
            .ok_or_else(|| anyhow!("unknown upstream: {server}"))?
            .clone();

        let _guard = sess.call_lock.lock().await;
        let client_guard = sess.client.lock().await;
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => {
                let msg = format!("upstream '{server}' has no client");
                sess.mark_err(&msg);
                return Err(anyhow!(msg));
            }
        };

        let live = match tokio::time::timeout(self.call_timeout, client.list_all_prompts()).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                let err = anyhow!(e).context(format!("upstream '{server}' prompts/list failed"));
                sess.mark_err(format!("{err:#}"));
                return Err(err);
            }
            Err(_) => {
                let msg = format!("upstream '{server}' prompts/list timed out");
                sess.mark_err(&msg);
                return Err(anyhow!(msg));
            }
        };

        let resolved = resolve_prompts(server, live);
        info!(
            upstream = %server,
            count = resolved.len(),
            "refreshed upstream prompts cache"
        );
        sess.prompts.store(Arc::new(resolved));
        sess.mark_ok();
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
                let last_ok = s.last_ok_unix_ms.load(Ordering::Relaxed);
                let spec = s.spec.load();
                UpstreamStatus {
                    name: kv.key().clone(),
                    description: spec.description.clone(),
                    transport: format!("{:?}", spec.transport).to_ascii_lowercase(),
                    connected: s.connected.load(Ordering::Relaxed),
                    tool_count: s.resolved.load().len(),
                    prompt_count: s.prompts.load().len(),
                    last_error: s.last_error.load_full().as_ref().clone(),
                    last_ok_unix_ms: (last_ok > 0).then_some(last_ok),
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
            .map(|kv| (**kv.value().spec.load()).clone())
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

    /// Snapshot of raw + resolved tools for rollback after a failed schema rebuild.
    #[allow(clippy::type_complexity)]
    pub fn tools_snapshot(&self, server: &str) -> Option<(Arc<Vec<Tool>>, Arc<Vec<ResolvedTool>>)> {
        let sess = self.sessions.get(server)?;
        Some((sess.tools.load_full(), sess.resolved.load_full()))
    }

    /// Restore tools caches after a failed post-refresh schema rebuild.
    pub fn restore_tools(
        &self,
        server: &str,
        tools: Arc<Vec<Tool>>,
        resolved: Arc<Vec<ResolvedTool>>,
    ) -> bool {
        let Some(sess) = self.sessions.get(server) else {
            return false;
        };
        sess.tools.store(tools);
        sess.resolved.store(resolved);
        true
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
        let _guard = sess.call_lock.lock().await;
        let client_guard = sess.client.lock().await;
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => {
                let msg = format!("upstream '{server}' has no client");
                sess.mark_err(&msg);
                return Err(anyhow!(msg));
            }
        };

        let live_tools =
            match tokio::time::timeout(self.call_timeout, client.list_all_tools()).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    let err = anyhow!(e).context(format!("upstream '{server}' tools/list failed"));
                    sess.mark_err(format!("{err:#}"));
                    return Err(err);
                }
                Err(_) => {
                    let msg = format!("upstream '{server}' tools/list timed out");
                    sess.mark_err(&msg);
                    return Err(anyhow!(msg));
                }
            };

        let sidecar = resolve_sidecar(&sess.spec.load(), spec_dir)?;
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
        sess.mark_ok();
        Ok(())
    }

    /// Number of sessions currently marked connected.
    pub fn connected_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|kv| kv.value().connected.load(Ordering::Relaxed))
            .count()
    }

    /// True if at least one session is connected.
    ///
    /// Note: an **empty** pool returns `false`. Callers that mean "no work to
    /// do" (zero enabled upstreams in the registry) must check the registry
    /// separately — see `/ready`.
    pub fn any_connected(&self) -> bool {
        self.connected_count() > 0
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
    pub last_ok_unix_ms: Option<u64>,
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

type UpstreamClient = RunningService<RoleClient, ForwardingClient>;

/// Connect to one upstream (stdio or HTTP) and complete the MCP handshake.
///
/// Returns the running client plus the caller-identity slot shared with the
/// HTTP transport (stdio gets an unused slot for a uniform session shape).
async fn connect_upstream(
    spec: &UpstreamSpec,
    bus: Arc<Bus>,
) -> Result<(UpstreamClient, identity::CallerSlot)> {
    let handler = ForwardingClient::new(spec.name.clone(), bus);
    let caller_slot = identity::new_caller_slot();

    match spec.transport {
        vmcp_registry::UpstreamTransport::Http => {
            let url = spec.url.clone().context("http upstream requires `url`")?;
            debug!(name = %spec.name, %url, "connecting http upstream");
            let mut config = StreamableHttpClientTransportConfig::with_uri(url);
            if let Some(token) = &spec.bearer {
                config = config.auth_header(token.clone());
            }
            let http = identity::IdentityHttpClient::new(caller_slot.clone())?;
            let transport = StreamableHttpClientTransport::with_client(http, config);
            let client = handler
                .serve(transport)
                .await
                .context("MCP handshake with http upstream")?;
            Ok((client, caller_slot))
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
            let client = handler
                .serve(transport)
                .await
                .context("MCP handshake with upstream")?;
            Ok((client, caller_slot))
        }
    }
}

fn cached_tools_from_live(live_tools: &[Tool]) -> Vec<CachedTool> {
    live_tools
        .iter()
        .map(|t| CachedTool {
            name: t.name.to_string(),
            description: t.description.as_ref().map(|s| s.to_string()),
            input_schema: serde_json::to_value(&t.input_schema)
                .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
            read_only: tool_read_only_hint(t),
            task_support: tool_task_support_hint(t),
        })
        .collect()
}

/// One-shot connect → `tools/list` → disconnect. Used by `vmcp add mcp` to
/// generate `specs/<server>.json` without keeping a long-lived pool session.
pub async fn probe_upstream_tools(spec: UpstreamSpec) -> Result<Vec<CachedTool>> {
    let bus = Bus::new(16);
    let (client, _slot) = connect_upstream(&spec, bus).await?;
    let live_tools = client
        .list_all_tools()
        .await
        .context("upstream tools/list")?;
    // Dropping `client` closes the transport / kills stdio child (kill_on_drop).
    drop(client);
    Ok(cached_tools_from_live(&live_tools))
}

/// Spawn a single upstream. Public so tests can do one-shot spawns.
pub async fn spawn_one(
    spec: UpstreamSpec,
    bus: Arc<Bus>,
    spec_dir: Option<&std::path::Path>,
) -> Result<UpstreamSession> {
    let (client, caller_slot) = connect_upstream(&spec, bus).await?;

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
    let cached = cached_tools_from_live(&live_tools);
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

    let (connected, last_error, last_ok_unix_ms) = new_session_fields(true);
    Ok(UpstreamSession {
        spec: ArcSwap::from_pointee(spec),
        client: Mutex::new(Some(client)),
        tools: ArcSwap::from_pointee(live_tools),
        resolved: ArcSwap::from_pointee(resolved),
        prompts: ArcSwap::from_pointee(resolved_prompts),
        call_lock: Mutex::new(()),
        caller_slot,
        connected,
        last_error,
        last_ok_unix_ms,
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

/// rmcp 3 dropped the Tool `execution.taskSupport` / SEP-1686 hint.
/// Sidecar overrides still apply after this default.
fn tool_task_support_hint(_tool: &Tool) -> vmcp_registry::TaskSupportHint {
    vmcp_registry::TaskSupportHint::Forbidden
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

    #[allow(deprecated)]
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

#[cfg(test)]
mod status_tests {
    use super::*;
    use vmcp_notify::Bus;

    #[test]
    fn empty_pool_has_zero_connected() {
        let pool = UpstreamPool::empty_for_test(Bus::new(8));
        assert_eq!(pool.connected_count(), 0);
        assert!(!pool.any_connected());
        assert!(pool.status_snapshot().is_empty());
    }

    #[test]
    fn synthetic_session_reports_connected_and_tool_count() {
        let pool = UpstreamPool::empty_for_test(Bus::new(8));
        pool.insert_synthetic_for_test(
            "alpha",
            Some("desc".into()),
            vec![ResolvedTool {
                server: "alpha".into(),
                name: "echo".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
                task_support: vmcp_registry::TaskSupportHint::Forbidden,
            }],
        );
        assert!(pool.any_connected());
        let snap = pool.status_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "alpha");
        assert!(snap[0].connected);
        assert_eq!(snap[0].tool_count, 1);
        assert!(snap[0].last_error.is_none());
        assert!(snap[0].last_ok_unix_ms.is_some());
    }

    #[test]
    fn mark_err_then_mark_ok_updates_status() {
        let pool = UpstreamPool::empty_for_test(Bus::new(8));
        pool.insert_synthetic_for_test("s", None, vec![]);
        let sess = pool.sessions.get("s").unwrap().clone();
        sess.mark_err("boom");
        let bad = pool.status_snapshot();
        assert!(!bad[0].connected);
        assert_eq!(bad[0].last_error.as_deref(), Some("boom"));
        assert!(!pool.any_connected());

        sess.mark_ok();
        let good = pool.status_snapshot();
        assert!(good[0].connected);
        assert!(good[0].last_error.is_none());
        assert!(pool.any_connected());
    }

    #[test]
    fn set_description_updates_without_respawn() {
        let pool = UpstreamPool::empty_for_test(Bus::new(8));
        pool.insert_synthetic_for_test("s", Some("old".into()), vec![]);
        assert!(pool.set_description("s", Some("new".into())));
        assert_eq!(pool.description_of("s").as_deref(), Some("new"));
        assert_eq!(pool.specs_snapshot()[0].description.as_deref(), Some("new"));
    }

    #[test]
    fn spec_requires_respawn_on_url_change() {
        let a = UpstreamSpec {
            name: "x".into(),
            description: None,
            transport: vmcp_registry::UpstreamTransport::Http,
            url: Some("http://a/mcp".into()),
            bearer: None,
            command: String::new(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
            forward_identity: false,
        };
        let mut b = a.clone();
        assert!(!spec_requires_respawn(&a, &b));
        b.url = Some("http://b/mcp".into());
        assert!(spec_requires_respawn(&a, &b));
    }
}
