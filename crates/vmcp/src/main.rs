//! vmcp — Virtual MCP gateway binary.
//!
//! Wires every library crate together into one HTTP listener:
//!   - Axum owns the socket
//!   - `/.well-known/oauth-*`, `/register`, `/authorize`, `/consent`, `/token`,
//!     `/.well-known/jwks.json` are unauthenticated
//!   - `/mcp` is fronted by `require_bearer` and serves the rmcp
//!     `StreamableHttpService` with our two-tool surface
//!   - `/health` is open and returns "ok"

#![allow(clippy::result_large_err)]

mod mcp_capture;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{middleware, routing::get, Router};
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use vmcp_auth::{
    build_router as auth_router, jwks::JwksManager, password, require_bearer, state::AuthState,
};
use vmcp_graphql::{build_schema, CapMode as GqlCapMode, SchemaLimits};
use vmcp_notify::Bus;
use vmcp_registry::{load_registry, save_lock_atomic, ToolsLock};
use vmcp_server::{load_skills, ProxyServer, VmcpServer};
use vmcp_upstream::UpstreamPool;

#[derive(Parser, Debug)]
#[command(name = "vmcp", version, about = "Virtual MCP gateway")]
struct Cli {
    /// Path to vmcp.toml (default ./vmcp.toml; env VMCP_CONFIG overrides).
    #[arg(short, long, env = "VMCP_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the gateway (default if no subcommand given).
    Serve,
    /// Generate an argon2id hash from a master password.
    HashPassword {
        /// The password to hash. If absent, read from stdin.
        #[arg(long)]
        password: Option<String>,
    },
    /// Print the resolved config and exit.
    PrintConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::HashPassword { password } => {
            let p = match password {
                Some(p) => p,
                None => {
                    let mut buf = String::new();
                    std::io::stdin().read_line(&mut buf)?;
                    buf.trim().to_string()
                }
            };
            let h = password::hash_password(&p).context("hash failed")?;
            println!("{h}");
            return Ok(());
        }
        Command::PrintConfig => {
            let cfg = vmcp_config::load(cli.config.as_deref())?;
            println!("{}", toml::to_string_pretty(&cfg)?);
            return Ok(());
        }
        Command::Serve => {}
    }

    let cfg = vmcp_config::load(cli.config.as_deref())?;
    info!(host = %cfg.host, port = cfg.port, "vmcp starting");

    // 1. Notification bus.
    let bus: Arc<Bus> = Bus::new(cfg.notif_ring_max);

    // 2. Upstream pool. Parallel fan-out.
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

    // 3. Lock file: build from snapshot.
    let lock = ToolsLock::new(pool.snapshot_lock());
    save_lock_atomic(&cfg.lock_path, &lock).context("save tools lock")?;

    // 4. GraphQL schema.
    let entries = pool.all_resolved();
    let schema = build_schema(
        entries,
        pool.clone(),
        SchemaLimits {
            max_depth: cfg.gql.max_depth,
            max_complexity: cfg.gql.max_complexity,
            max_response_bytes: cfg.gql.max_response_bytes,
            response_cap_mode: match cfg.gql.response_cap_mode {
                vmcp_config::CapMode::Error => GqlCapMode::Error,
                vmcp_config::CapMode::Truncate => GqlCapMode::Truncate,
            },
        },
    )
    .map_err(|e| anyhow::anyhow!("build schema: {e}"))?;
    let schema_swap: Arc<ArcSwap<async_graphql::dynamic::Schema>> =
        Arc::new(ArcSwap::from_pointee(schema));

    // 5. Skills — operator-curated MCP prompts loaded from `skills_dir`.
    //    Held in an ArcSwap so the admin CRUD API can hot-swap the in-memory
    //    snapshot after writing the YAML file to disk, without restarting.
    let skills_vec = load_skills(&cfg.skills_dir).context("load skills")?;
    info!(count = skills_vec.len(), dir = ?cfg.skills_dir, "skills loaded");
    let skills: vmcp_server::SkillsHandle =
        Arc::new(ArcSwap::from_pointee(skills_vec));

    // 6. MCP server (cloneable; shared via Arc-style move into the rmcp factory).
    let vmcp_server = VmcpServer::new(schema_swap.clone(), pool.clone(), skills.clone());

    // 7. Auth state + JWKS rotation task.
    let jwks = JwksManager::new_with_fresh(&cfg.auth.jwt_kid)?;
    let _rotation = jwks.clone().spawn_rotation_task(
        Duration::from_secs(cfg.auth.jwks_rotate_secs),
        cfg.auth.jwt_kid.clone(),
    );
    // RFC 9728 protected-resource metadata describes a resource server as a
    // whole, not individual paths. Using the origin (without /mcp suffix) as
    // the audience lets one token cover every Bearer-protected route — /mcp
    // (GraphQL semantic) and /mcp-proxy (transparent passthrough) — and
    // matches strict clients (opencode, browser-based MCP UAs) that compare
    // the protected-resource `resource` field against the request origin.
    // Previously this was `{base}{mcp_path}` which advertised /mcp only and
    // made /mcp-proxy unreachable behind OAuth ("resource mismatch" 401).
    let resource_audience = cfg.public_base_url.trim_end_matches('/').to_string();
    let auth_state = AuthState::new(
        jwks.clone(),
        cfg.effective_issuer().to_string(),
        resource_audience,
        cfg.auth.token_ttl_secs,
        cfg.auth.master_password_argon2.clone(),
    );

    // 7b. Recorder + live session registry. Part A owns the config keys; if
    // its branch hasn't merged the build of this binary will fail in a
    // pinpointable way ("field `recorder` of `Settings`") — see the sibling
    // task's brief.
    let recorder = vmcp_server::recorder::Recorder::new(
        cfg.recorder.sessions_dir.clone(),
        cfg.recorder.redact_keys.clone(),
    );
    recorder.startup_cleanup().await?;
    let registry = std::sync::Arc::new(vmcp_server::sessions::SessionRegistry::new());

    {
        let r = registry.clone();
        let interval_secs = cfg.recorder.gc_interval_secs;
        let idle_ttl_ms = cfg.recorder.idle_ttl_secs * 1000;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                r.gc(now, idle_ttl_ms);
            }
        });
    }

    // 8. HTTP router. rmcp HTTP service is mounted under /mcp behind require_bearer.
    //
    // rmcp's StreamableHttpService default allowed_hosts is only loopback
    // (localhost, 127.0.0.1, ::1) to block DNS rebinding against locally-
    // running dev servers. Public deployments must add their own hostname
    // or every request gets rejected with "disallowed Host header" before
    // even hitting the auth middleware. Extract it from public_base_url
    // so operators only configure the URL once.
    let mut allowed_hosts: Vec<String> = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
    ];
    if let Ok(url) = url::Url::parse(&cfg.public_base_url) {
        if let Some(h) = url.host_str() {
            allowed_hosts.push(h.to_string());
            if let Some(p) = url.port() {
                allowed_hosts.push(format!("{h}:{p}"));
            }
        }
    }
    let rmcp_config =
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts.clone());
    let rmcp_service = StreamableHttpService::new(
        move || Ok(vmcp_server.clone()),
        LocalSessionManager::default().into(),
        rmcp_config,
    );
    // route_layer applies only to routes added BEFORE it and would no-op
    // here because we only register a fallback_service. Use plain .layer()
    // so the auth middleware wraps the rmcp service too.
    //
    // Order matters: `require_bearer` runs FIRST so claims are inserted into
    // the request extensions before `capture_mcp` reads them, and the
    // recorder never sees unauthenticated traffic.
    let mcp_router = Router::new()
        .fallback_service(rmcp_service)
        .layer(middleware::from_fn_with_state(
            mcp_capture::CaptureState {
                recorder: recorder.clone(),
                registry: registry.clone(),
                endpoint: cfg.mcp_path.clone(),
            },
            mcp_capture::capture_mcp,
        ))
        .layer(middleware::from_fn_with_state(
            auth_state.clone(),
            require_bearer,
        ));

    // Operator admin (HTTP Basic against the same master hash that gates
    // /consent). Mounted under /admin. Skills CRUD here is no longer read-
    // only — the admin owns `skills_dir` and `.store()`s a fresh Vec into the
    // shared ArcSwap after every write.
    let admin_state = vmcp_admin::AdminState::new(
        pool.clone(),
        schema_swap.clone(),
        bus.clone(),
        skills.clone(),
        cfg.skills_dir.clone(),
        cfg.auth.master_password_argon2.clone(),
        auth_state.clone(),
        registry.clone(),
        recorder.clone(),
    );
    let admin_router = vmcp_admin::router(admin_state);

    let mut app = Router::new()
        .merge(auth_router(auth_state.clone()))
        .route("/health", get(|| async { "ok" }))
        .nest("/admin", admin_router)
        .nest(&cfg.mcp_path, mcp_router);

    // Optional transparent MCP proxy on a side endpoint. Same auth + capture
    // middleware as /mcp; the recorder writes separate sessions per path.
    if cfg.proxy.enabled {
        let proxy_server = ProxyServer::new(pool.clone());
        let proxy_rmcp_config =
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts.clone());
        let proxy_rmcp_service = StreamableHttpService::new(
            move || Ok(proxy_server.clone()),
            LocalSessionManager::default().into(),
            proxy_rmcp_config,
        );
        let proxy_router = Router::new()
            .fallback_service(proxy_rmcp_service)
            .layer(middleware::from_fn_with_state(
                mcp_capture::CaptureState {
                    recorder: recorder.clone(),
                    registry: registry.clone(),
                    endpoint: cfg.proxy.mcp_path.clone(),
                },
                mcp_capture::capture_mcp,
            ))
            .layer(middleware::from_fn_with_state(
                auth_state.clone(),
                require_bearer,
            ));
        info!(path = %cfg.proxy.mcp_path, "proxy mode enabled");
        app = app.nest(&cfg.proxy.mcp_path, proxy_router);
    }

    // 9. Listener. `into_make_service_with_connect_info::<SocketAddr>` makes
    // peer IP available to `ConnectInfo<SocketAddr>` extractors (used by the
    // admin rate limiter).
    let addr = std::net::SocketAddr::new(cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,h2=warn,rmcp=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
