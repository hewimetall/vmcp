//! Shared gateway boot: upstream pool, GraphQL schema, skills, MCP server surface.
//!
//! Separates core wiring from HTTP ingress (`serve_http`) so transport setup
//! cannot drift from pool/schema construction.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use tracing::{error, info, warn};
use vmcp_config::{CapMode, Settings};
use vmcp_graphql::{build_schema_with_prompts, CapMode as GqlCapMode, SchemaLimits};
use vmcp_notify::Bus;
use vmcp_registry::{load_registry, save_lock_atomic, ToolsLock};
use vmcp_server::{
    collect_task_allowlist, load_skills, prompt_source_handlers, SchemaHandle, SkillsHandle,
    TaskRunner, VmcpServer,
};
use vmcp_upstream::UpstreamPool;

/// Everything the HTTP ingress needs after config is loaded.
pub struct BootContext {
    pub cfg: Settings,
    // `bus` is consumed by the admin UI (feature `admin`).
    #[cfg_attr(not(feature = "admin"), allow(dead_code))]
    pub bus: Arc<Bus>,
    pub pool: Arc<UpstreamPool>,
    /// Shared with MCP server + registry hot-reload (+ admin when enabled).
    pub schema_swap: SchemaHandle,
    pub skills: SkillsHandle,
    pub vmcp_server: VmcpServer,
}

/// Spin up the shared gateway core. Does not start any ingress transport.
pub async fn boot(cfg: Settings) -> Result<BootContext> {
    let bus: Arc<Bus> = Bus::new(cfg.notif_ring_max);

    let registry = load_registry(&cfg.registry_path)?;
    let (pool, spawn_failures) = UpstreamPool::spawn_all(
        &registry,
        bus.clone(),
        Some(&cfg.spec_dir),
        Duration::from_millis(cfg.upstream.spawn_timeout_ms),
        Duration::from_millis(cfg.upstream.call_timeout_ms),
    )
    .await;
    for f in &spawn_failures {
        error!(upstream = %f.name, error = %f.error, "upstream spawn failed");
    }
    let pool = Arc::new(pool);
    info!(upstreams = pool.names().len(), "upstream pool ready");

    let lock = ToolsLock::new(pool.snapshot_lock());
    save_lock_atomic(&cfg.lock_path, &lock).context("save tools lock")?;

    let skills_vec = load_skills(&cfg.skills_dir).context("load skills")?;
    info!(count = skills_vec.len(), dir = ?cfg.skills_dir, "skills loaded");
    let skills: SkillsHandle = Arc::new(ArcSwap::from_pointee(skills_vec));

    // Local YAML skills always appear in GraphQL. Upstream prompts
    // (`{server}__{name}`) are included only when `[proxy]` is on — same
    // flag as the `/mcp-proxy` surface (tools + prompts passthrough).
    let prompt_handlers = prompt_source_handlers(skills.clone(), pool.clone(), cfg.proxy.enabled);
    let entries = pool.all_resolved();
    let schema = build_schema_with_prompts(
        entries,
        pool.clone(),
        SchemaLimits {
            max_depth: cfg.gql.max_depth,
            max_complexity: cfg.gql.max_complexity,
            max_response_bytes: cfg.gql.max_response_bytes,
            response_cap_mode: match cfg.gql.response_cap_mode {
                CapMode::Error => GqlCapMode::Error,
                CapMode::Truncate => GqlCapMode::Truncate,
            },
        },
        Some(prompt_handlers),
    )
    .map_err(|e| anyhow::anyhow!("build schema: {e}"))?;
    let schema_swap: SchemaHandle = Arc::new(ArcSwap::from_pointee(schema));

    // Native MCP Tasks (SEP-1686) — SQLite-backed `run_task` for task-capable
    // upstream tools only (execution.taskSupport / sidecar task_support).
    let task_runner = if cfg.tasks.enabled {
        let allowlist = collect_task_allowlist(&pool);
        if allowlist.is_empty() {
            warn!(
                "tasks.enabled but no upstream tools advertise taskSupport; \
                 run_task will not be registered"
            );
            None
        } else {
            info!(
                db = %cfg.tasks.db_path.display(),
                tools = allowlist.len(),
                "native MCP tasks enabled (SQLite TaskStore)"
            );
            Some(Arc::new(
                TaskRunner::new(
                    pool.clone(),
                    cfg.tasks.db_path.clone(),
                    allowlist,
                    cfg.tasks.max_concurrent,
                    cfg.tasks.task_ttl_ms,
                    cfg.tasks.poll_interval_ms,
                )
                .context("open tasks sqlite db")?,
            ))
        }
    } else {
        None
    };

    let vmcp_server = VmcpServer::with_tasks(
        schema_swap.clone(),
        pool.clone(),
        skills.clone(),
        task_runner,
    );

    // Push upstream MCP events to connected clients (pull stays available via
    // `query_graphql { notifications }`).
    vmcp_server.spawn_notification_forwarder();

    Ok(BootContext {
        cfg,
        bus,
        pool,
        schema_swap,
        skills,
        vmcp_server,
    })
}
