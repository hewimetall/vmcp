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

struct RegistryReloadInner {
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
            Some(old)
                if spec_requires_respawn(old, spec) || old.description != spec.description =>
            {
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
            Some(_) => {}
        }
    }

    // Rebuild GraphQL schema from the new pool snapshot.
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
    .map_err(|e| anyhow::anyhow!("rebuild schema after registry reload: {e}"))?;
    // Same ArcSwap as admin / MCP server surface.
    inner.vmcp_server.swap_schema(schema);

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

    Ok(json!({
        "added": added,
        "removed": removed,
        "replaced": replaced,
        "failed": failed,
        "tool_count": tool_count,
        "upstreams": statuses,
    }))
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

/// Spawn a debounced file watcher that reloads the registry on change.
/// Returns the watcher guard (keep alive) or an error.
pub fn spawn_registry_watcher(
    handle: RegistryReloadHandle,
    registry_path: PathBuf,
) -> anyhow::Result<vmcp_watch::FileWatcher> {
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
    vmcp_watch::spawn_file_watcher(&registry_path, move || {
        pending_cb.store(true, Ordering::SeqCst);
    })
}
