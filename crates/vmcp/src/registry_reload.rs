//! Hot-reload / reconcile of `registry.json` into the live upstream pool + GraphQL schema.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use vmcp_config::{CapMode, Settings};
use vmcp_graphql::{build_schema_with_prompts, CapMode as GqlCapMode, SchemaLimits};
use vmcp_registry::{load_registry, save_lock_atomic, ToolsLock, UpstreamSpec};
use vmcp_server::{prompt_source_handlers, SkillsHandle, VmcpServer};
use vmcp_upstream::{spawn_one, spec_requires_respawn, UpstreamPool};

/// Shared handle used by the file watcher and `POST /api/v1/upstreams/reload`.
#[derive(Clone)]
pub struct RegistryReloadHandle {
    inner: Arc<RegistryReloadInner>,
}

pub(crate) struct RegistryReloadInner {
    lock: Mutex<()>,
    cfg: Settings,
    pool: Arc<UpstreamPool>,
    skills: SkillsHandle,
    vmcp_server: VmcpServer,
}

impl RegistryReloadHandle {
    pub fn new(
        cfg: Settings,
        pool: Arc<UpstreamPool>,
        skills: SkillsHandle,
        vmcp_server: VmcpServer,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryReloadInner {
                lock: Mutex::new(()),
                cfg,
                pool,
                skills,
                vmcp_server,
            }),
        }
    }

    pub fn pool(&self) -> Arc<UpstreamPool> {
        self.inner.pool.clone()
    }

    /// Reconcile disk registry with the live pool and rebuild the GraphQL schema.
    pub async fn reload(&self) -> Result<Value> {
        let _guard = self.inner.lock.lock().await;
        reconcile(&self.inner).await
    }

    /// Upstream `tools/list_changed`: refresh that server's tool cache and rebuild schema.
    pub async fn handle_tools_changed(&self, source: &str) {
        let _guard = self.inner.lock.lock().await;
        if let Err(e) = self
            .inner
            .pool
            .refresh_tools(source, Some(self.inner.cfg.spec_dir.as_path()))
            .await
        {
            warn!(
                upstream = %source,
                error = %e,
                "failed to refresh tools after list_changed"
            );
            return;
        }
        if let Err(e) = rebuild_graphql_schema(&self.inner).await {
            warn!(
                upstream = %source,
                error = %e,
                "failed to rebuild schema after tools/list_changed"
            );
            return;
        }
        // Keep run_task allowlist in sync with sidecar-/list-merged taskSupport.
        if let Some(runner) = self.inner.vmcp_server.task_runner() {
            runner.replace_allowlist(vmcp_server::collect_task_allowlist(&self.inner.pool));
        }
        info!(upstream = %source, "tools/list_changed applied (cache + schema)");
    }
}

/// Rebuild GraphQL schema from the live pool into the shared ArcSwap.
pub(crate) async fn rebuild_graphql_schema(inner: &RegistryReloadInner) -> Result<()> {
    let prompt_handlers = prompt_source_handlers(
        inner.skills.clone(),
        inner.pool.clone(),
        inner.cfg.proxy.enabled,
    );
    let entries = inner.pool.all_resolved();
    let schema = build_schema_with_prompts(
        entries,
        inner.pool.clone(),
        SchemaLimits {
            max_depth: inner.cfg.gql.max_depth,
            max_complexity: inner.cfg.gql.max_complexity,
            max_response_bytes: inner.cfg.gql.max_response_bytes,
            response_cap_mode: match inner.cfg.gql.response_cap_mode {
                CapMode::Error => GqlCapMode::Error,
                CapMode::Truncate => GqlCapMode::Truncate,
            },
        },
        Some(prompt_handlers),
    )
    .map_err(|e| anyhow::anyhow!("rebuild schema: {e}"))?;
    inner.vmcp_server.swap_schema(schema);
    Ok(())
}

async fn reconcile(inner: &RegistryReloadInner) -> Result<Value> {
    let registry = load_registry(&inner.cfg.registry_path)
        .with_context(|| format!("load registry {}", inner.cfg.registry_path.display()))?;

    let desired: HashMap<String, UpstreamSpec> = registry
        .upstreams
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.name.clone(), s))
        .collect();

    let current_specs = inner.pool.specs_snapshot();
    let current_names: HashSet<String> = current_specs.iter().map(|s| s.name.clone()).collect();
    let current_by_name: HashMap<String, UpstreamSpec> = current_specs
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut replaced = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    // Removals (or disabled).
    for name in current_names.difference(&desired.keys().cloned().collect::<HashSet<_>>()) {
        inner.pool.remove(name).await;
        removed.push(name.clone());
    }

    let spawn_timeout = Duration::from_millis(inner.cfg.upstream.spawn_timeout_ms);
    let spec_dir = Some(inner.cfg.spec_dir.as_path());

    for (name, spec) in &desired {
        match current_by_name.get(name) {
            None => match spawn_with_timeout(spec.clone(), inner, spawn_timeout, spec_dir).await {
                Ok(sess) => {
                    inner.pool.upsert(name.clone(), sess).await;
                    added.push(name.clone());
                }
                Err(e) => {
                    error!(upstream = %name, error = %e, "registry reload: add failed");
                    failed.push(json!({ "name": name, "error": e.to_string(), "op": "add" }));
                }
            },
            Some(old) if spec_requires_respawn(old, spec) => {
                match spawn_with_timeout(spec.clone(), inner, spawn_timeout, spec_dir).await {
                    Ok(sess) => {
                        inner.pool.upsert(name.clone(), sess).await;
                        replaced.push(name.clone());
                    }
                    Err(e) => {
                        error!(upstream = %name, error = %e, "registry reload: replace failed");
                        failed.push(json!({
                            "name": name,
                            "error": e.to_string(),
                            "op": "replace"
                        }));
                    }
                }
            }
            Some(old) if old.description != spec.description => {
                // G15: description-only — no cancel/spawn.
                let _ = inner.pool.set_description(name, spec.description.clone());
            }
            Some(_) => {}
        }
    }

    // Rebuild GraphQL schema from the new pool snapshot.
    // Note: pool mutations above are already applied. If schema rebuild fails,
    // return Err so the operator retries; pool stays at the new desired state
    // (full transactional rollback of live sessions is not supported).
    rebuild_graphql_schema(inner)
        .await
        .context("rebuild schema after registry reload")?;

    // Keep run_task allowlist in sync after successful schema rebuild.
    if let Some(runner) = inner.vmcp_server.task_runner() {
        let allow = vmcp_server::collect_task_allowlist(&inner.pool);
        info!(
            tools = allow.len(),
            "updated run_task allowlist after reload"
        );
        runner.replace_allowlist(allow);
    }

    let lock = ToolsLock::new(inner.pool.snapshot_lock());
    if let Err(e) = save_lock_atomic(&inner.cfg.lock_path, &lock) {
        warn!(error = %e, "failed to rewrite tools.lock.json after reload");
    }

    // Notify MCP clients that tools changed.
    inner.pool.bus().publish(
        "vmcp",
        "notifications/tools/list_changed",
        json!({ "reason": "registry_reload" }),
    );

    let statuses: Vec<Value> = inner
        .pool
        .status_snapshot()
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "transport": s.transport,
                "connected": s.connected,
                "tool_count": s.tool_count,
                "prompt_count": s.prompt_count,
                "last_error": s.last_error,
            })
        })
        .collect();

    let tool_count: usize = statuses
        .iter()
        .map(|s| s["tool_count"].as_u64().unwrap_or(0) as usize)
        .sum();

    info!(
        added = added.len(),
        removed = removed.len(),
        replaced = replaced.len(),
        failed = failed.len(),
        tool_count,
        "registry reconcile complete"
    );

    let (registry_sha256, mtime_unix_ms) = registry_file_meta(&inner.cfg.registry_path);

    Ok(json!({
        "added": added,
        "removed": removed,
        "replaced": replaced,
        "failed": failed,
        "tool_count": tool_count,
        "upstreams": statuses,
        "registry_sha256": registry_sha256,
        "mtime_unix_ms": mtime_unix_ms,
    }))
}

fn registry_file_meta(path: &std::path::Path) -> (Option<String>, Option<u64>) {
    use sha2::{Digest, Sha256};
    let meta = std::fs::metadata(path).ok();
    let mtime = meta.and_then(|m| m.modified().ok()).and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64)
    });
    let sha = std::fs::read(path).ok().map(|bytes| {
        let digest = Sha256::digest(&bytes);
        format!("{digest:x}")
    });
    (sha, mtime)
}

async fn spawn_with_timeout(
    spec: UpstreamSpec,
    inner: &RegistryReloadInner,
    spawn_timeout: Duration,
    spec_dir: Option<&std::path::Path>,
) -> Result<vmcp_upstream::UpstreamSession> {
    let bus = inner.pool.bus();
    let name = spec.name.clone();
    match tokio::time::timeout(spawn_timeout, spawn_one(spec, bus, spec_dir)).await {
        Ok(Ok(sess)) => Ok(sess),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("spawn timed out for upstream {name}")),
    }
}

/// Guards that must stay alive for registry hot-reload (inotify + mtime poll).
#[allow(dead_code)] // fields exist to keep watcher/poller running
pub struct RegistryWatchGuards {
    pub watcher: vmcp_watch::FileWatcher,
    pub poller: tokio::task::JoinHandle<()>,
}

/// Spawn a debounced file watcher + mtime poller that reloads the registry on
/// change. Recursive watch + 2s mtime poll cover Kubernetes ConfigMap
/// `..data` symlink updates (G03). Prefer `POST /api/v1/upstreams/reload`
/// after operator applies a CM for deterministic reconcile.
pub fn spawn_registry_watcher(
    handle: RegistryReloadHandle,
    registry_path: PathBuf,
) -> anyhow::Result<RegistryWatchGuards> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;

    let pending = StdArc::new(AtomicBool::new(false));
    let pending_flag = pending.clone();
    let handle_bg = handle.clone();

    // Debounce worker: when flag is set, wait 300ms then reload once.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if pending_flag.swap(false, Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(300)).await;
                // Collapse bursts during the debounce window.
                pending_flag.store(false, Ordering::SeqCst);
                if let Err(e) = handle_bg.reload().await {
                    error!(error = %e, "registry hot-reload failed");
                }
            }
        }
    });

    let pending_cb = pending.clone();
    let watcher = vmcp_watch::spawn_file_watcher_recursive(&registry_path, move || {
        pending_cb.store(true, Ordering::SeqCst);
    })?;

    let pending_poll = pending.clone();
    let poll_path = registry_path;
    let poller = tokio::spawn(async move {
        let mut last = std::fs::metadata(&poll_path)
            .and_then(|m| m.modified())
            .ok();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let now = std::fs::metadata(&poll_path)
                .and_then(|m| m.modified())
                .ok();
            if now != last {
                last = now;
                tracing::debug!(
                    path = %poll_path.display(),
                    "registry mtime poll detected change"
                );
                pending_poll.store(true, Ordering::SeqCst);
            }
        }
    });

    Ok(RegistryWatchGuards { watcher, poller })
}
