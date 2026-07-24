//! vmcp — Virtual MCP gateway binary.
//!
//! HTTP ingress only (`serve`): Axum + StreamableHttpService at `/mcp`, OAuth
//! bearer, optional admin UI and transparent proxy.
//!
//! Local stdio MCP hosts use [`vmcp-lite`](https://github.com/hewimetall/vmcp-lite)
//! (`uvx vmcp-lite-mcp`) instead.

#![allow(clippy::result_large_err)]

mod api_v1;
mod boot;
#[cfg(not(feature = "otel"))]
mod mcp_capture;
#[cfg(feature = "otel")]
mod mcp_otel;
mod registry_reload;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;
use vmcp_auth::{password, static_tokens};

use boot::BootContext;

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
    /// Run the HTTP gateway (default if no subcommand given).
    Serve,
    /// Generate an argon2id hash from a master password.
    HashPassword {
        /// The password to hash. If absent, read from stdin.
        #[arg(long)]
        password: Option<String>,
    },
    /// Pre-register an eternal opaque bearer token and append it to the static
    /// token file. The printed `vmcp_...` token never expires and bypasses the
    /// OAuth flow; point the server at the file via `auth.tokens_file`. Revoke
    /// by deleting its line from the file (hot-reloaded, no restart).
    PreReg {
        /// Client label; also used as the `client_id` ([A-Za-z0-9_-], <=128).
        #[arg(long)]
        name: String,
        /// OAuth scope to grant. Defaults to `mcp:use`.
        #[arg(long)]
        scope: Option<String>,
        /// Token file to append to (created if absent).
        #[arg(long, default_value = "./tokens.json")]
        out: PathBuf,
    },
    /// Print the resolved config and exit.
    PrintConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    // With `--features otel`, HTTP serve installs the OTEL+fmt subscriber
    // itself. Everything else gets the plain stderr fmt subscriber.
    #[cfg(feature = "otel")]
    {
        if !matches!(command, Command::Serve) {
            init_tracing();
        }
    }
    #[cfg(not(feature = "otel"))]
    init_tracing();

    match command {
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
        Command::PreReg { name, scope, out } => {
            let entry =
                static_tokens::generate_entry(&name, scope.as_deref()).context("generate token")?;
            static_tokens::append_atomic(&out, &entry).context("append token file")?;
            eprintln!(
                "pre-registered '{}' (scope {}) -> {}",
                entry.client_id,
                entry.scope,
                out.display()
            );
            println!("{}", entry.token);
            return Ok(());
        }
        Command::PrintConfig => {
            let cfg = vmcp_config::load(cli.config.as_deref())?;
            println!("{}", toml::to_string_pretty(&cfg)?);
            return Ok(());
        }
        Command::Serve => {
            let cfg = vmcp_config::load(cli.config.as_deref())?;
            #[cfg(feature = "otel")]
            let otel_store = {
                let store = vmcp_server::otel_file::SpanStore::new(
                    cfg.recorder.sessions_dir.clone(),
                    cfg.recorder.redact_keys.clone(),
                );
                let provider = init_otel_tracing(store.clone())?;
                // Keep provider alive for the process lifetime (flush on drop).
                std::mem::forget(provider);
                store
            };
            let ctx = boot::boot(cfg).await?;
            #[cfg(feature = "otel")]
            {
                serve_http(ctx, otel_store).await
            }
            #[cfg(not(feature = "otel"))]
            {
                serve_http(ctx).await
            }
        }
    }
}

/// HTTP ingress: Axum listener, OAuth, admin, optional proxy.
async fn serve_http(
    ctx: BootContext,
    #[cfg(feature = "otel")] recorder: std::sync::Arc<vmcp_server::otel_file::SpanStore>,
) -> Result<()> {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::{middleware, routing::get, Router};
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use tracing::{error, warn};
    use vmcp_auth::{
        build_router as auth_router, client_store::ClientStore, jwks::JwksManager,
        require_admin_scope, require_bearer, state::AuthState,
    };
    use vmcp_server::ProxyServer;
    use vmcp_watch::spawn_file_watcher;

    use crate::api_v1::{self, ApiV1State};
    use crate::registry_reload::{spawn_registry_watcher, RegistryReloadHandle};

    let cfg = &ctx.cfg;
    info!(host = %cfg.host, port = cfg.port, "vmcp starting");
    if !cfg.auth.enabled {
        warn!(
            target: "vmcp::auth",
            "auth.enabled = false — /mcp is unauthenticated and /admin is not mounted; do not expose to untrusted networks"
        );
    }

    let vmcp_server = ctx.vmcp_server.clone();
    let pool = ctx.pool.clone();
    let skills = ctx.skills.clone();
    #[cfg(feature = "admin")]
    let schema_swap = ctx.schema_swap.clone();
    #[cfg(feature = "admin")]
    let bus = ctx.bus.clone();

    let jwks = JwksManager::new_with_fresh(&cfg.auth.jwt_kid)?;
    let _rotation = if cfg.auth.enabled {
        Some(jwks.clone().spawn_rotation_task(
            Duration::from_secs(cfg.auth.jwks_rotate_secs),
            cfg.auth.jwt_kid.clone(),
        ))
    } else {
        None
    };

    // Canonical MCP resource indicator (RFC 8707). Cursor may send
    // `resource=https://host/mcp` or `…/mcp-proxy` depending on Server URL;
    // both mounts must be accepted audiences or token exchange fails after
    // DCR/consent.
    let base = cfg.public_base_url.trim_end_matches('/');
    let resource_audience = format!("{base}{}", cfg.mcp_path);
    let mut auth_state = AuthState::new(
        jwks.clone(),
        cfg.effective_issuer().to_string(),
        resource_audience,
        cfg.auth.token_ttl_secs,
        cfg.auth.master_password_argon2.clone(),
    );
    if cfg.proxy.enabled {
        auth_state =
            auth_state.with_extra_resource_audiences(vec![format!("{base}{}", cfg.proxy.mcp_path)]);
    }

    if cfg.auth.enabled {
        let store = ClientStore::open(&cfg.auth.clients_db_path).with_context(|| {
            format!(
                "open DCR clients sqlite db at {}",
                cfg.auth.clients_db_path.display()
            )
        })?;
        auth_state = auth_state.with_client_store(std::sync::Arc::new(store))?;
    }

    let _token_watcher = if cfg.auth.enabled {
        if let Some(path) = cfg.auth.tokens_file.clone() {
            let store = static_tokens::StaticTokenStore::load(&path)?;
            auth_state = auth_state.with_token_store(store.clone());
            let parent_ok = path
                .parent()
                .map(|p| p.as_os_str().is_empty() || p.exists())
                .unwrap_or(true);
            if parent_ok {
                let watch_path = path.clone();
                match spawn_file_watcher(&path, move || store.reload(&watch_path)) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        error!(error = %e, "failed to start token-file watcher; tokens won't hot-reload");
                        None
                    }
                }
            } else {
                error!(path = %path.display(), "token file's parent dir is missing; hot-reload disabled");
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    #[cfg(not(feature = "otel"))]
    let recorder = vmcp_server::recorder::Recorder::new(
        cfg.recorder.sessions_dir.clone(),
        cfg.recorder.redact_keys.clone(),
    );
    recorder.startup_cleanup().await?;
    // Durable session list: JSON files under sessions_dir/.registry/ so admin
    // sessions survive gateway restarts (dumps stay in client subdirs).
    let registry = Arc::new(vmcp_server::sessions::SessionRegistry::open(
        cfg.recorder.sessions_dir.clone(),
    )?);

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

    let mut allowed_hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
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
    let vmcp_server_for_mcp = vmcp_server.clone();
    let rmcp_service = StreamableHttpService::new(
        move || Ok(vmcp_server_for_mcp.clone()),
        LocalSessionManager::default().into(),
        rmcp_config,
    );

    #[cfg(feature = "otel")]
    let mut mcp_router =
        Router::new()
            .fallback_service(rmcp_service)
            .layer(middleware::from_fn_with_state(
                mcp_otel::CaptureState {
                    registry: registry.clone(),
                    endpoint: cfg.mcp_path.clone(),
                },
                mcp_otel::capture_mcp,
            ));
    #[cfg(not(feature = "otel"))]
    let mut mcp_router =
        Router::new()
            .fallback_service(rmcp_service)
            .layer(middleware::from_fn_with_state(
                mcp_capture::CaptureState {
                    recorder: recorder.clone(),
                    registry: registry.clone(),
                    endpoint: cfg.mcp_path.clone(),
                },
                mcp_capture::capture_mcp,
            ));
    if cfg.auth.enabled {
        mcp_router = mcp_router.layer(middleware::from_fn_with_state(
            auth_state.clone(),
            require_bearer,
        ));
    }

    let mut app = Router::new()
        .merge(auth_router(auth_state.clone()))
        .route("/health", get(|| async { "ok" }))
        .nest(&cfg.mcp_path, mcp_router);

    // Registry hot-reload (file watcher + /api/v1/upstreams/reload).
    let reload_handle = RegistryReloadHandle::new(
        ctx.cfg.clone(),
        pool.clone(),
        skills.clone(),
        vmcp_server.clone(),
    );
    let _registry_watcher = {
        let parent_ok = cfg
            .registry_path
            .parent()
            .map(|p| p.as_os_str().is_empty() || p.exists())
            .unwrap_or(true);
        if parent_ok {
            match spawn_registry_watcher(reload_handle.clone(), cfg.registry_path.clone()) {
                Ok(w) => Some(w),
                Err(e) => {
                    error!(error = %e, "failed to start registry watcher; hot-reload disabled");
                    None
                }
            }
        } else {
            error!(
                path = %cfg.registry_path.display(),
                "registry file's parent dir is missing; hot-reload disabled"
            );
            None
        }
    };

    // Operator control-plane (Bearer + mcp:admin). Parallel to `/admin` Basic SPA.
    if cfg.auth.enabled {
        let reload = reload_handle.clone();
        let pool_status = reload_handle.pool();
        let api_state = ApiV1State::new(auth_state.clone(), cfg.auth.tokens_file.clone())
            .with_reload(std::sync::Arc::new(move || {
                let reload = reload.clone();
                Box::pin(async move { reload.reload().await })
            }))
            .with_pool(pool_status);
        let api_router = api_v1::router(api_state)
            .layer(middleware::from_fn(require_admin_scope))
            .layer(middleware::from_fn_with_state(
                auth_state.clone(),
                require_bearer,
            ));
        app = app.nest("/api/v1", api_router);
    }

    #[cfg(feature = "admin")]
    {
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
        if cfg.auth.enabled {
            app = app.nest("/admin", admin_router);
        }
    }

    if cfg.proxy.enabled {
        let proxy_server = ProxyServer::new(pool.clone());
        let proxy_rmcp_config =
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts.clone());
        let proxy_rmcp_service = StreamableHttpService::new(
            move || Ok(proxy_server.clone()),
            LocalSessionManager::default().into(),
            proxy_rmcp_config,
        );
        #[cfg(feature = "otel")]
        let mut proxy_router = Router::new().fallback_service(proxy_rmcp_service).layer(
            middleware::from_fn_with_state(
                mcp_otel::CaptureState {
                    registry: registry.clone(),
                    endpoint: cfg.proxy.mcp_path.clone(),
                },
                mcp_otel::capture_mcp,
            ),
        );
        #[cfg(not(feature = "otel"))]
        let mut proxy_router = Router::new().fallback_service(proxy_rmcp_service).layer(
            middleware::from_fn_with_state(
                mcp_capture::CaptureState {
                    recorder: recorder.clone(),
                    registry: registry.clone(),
                    endpoint: cfg.proxy.mcp_path.clone(),
                },
                mcp_capture::capture_mcp,
            ),
        );
        if cfg.auth.enabled {
            proxy_router = proxy_router.layer(middleware::from_fn_with_state(
                auth_state.clone(),
                require_bearer,
            ));
        }
        info!(path = %cfg.proxy.mcp_path, "proxy mode enabled");
        app = app.nest(&cfg.proxy.mcp_path, proxy_router);
    }

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
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[cfg(feature = "otel")]
fn init_otel_tracing(
    store: std::sync::Arc<vmcp_server::otel_file::SpanStore>,
) -> Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use opentelemetry_sdk::Resource;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use vmcp_server::otel_file::DirSpanExporter;

    let exporter = DirSpanExporter::new(store);
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .with_resource(Resource::builder().with_service_name("vmcp").build())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let tracer = provider.tracer("vmcp");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,h2=warn,rmcp=info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(false);

    // Re-init: serve() calls this after the early init_tracing() in main.
    // Use try_init-compatible rebuild via set_global_default is already taken,
    // so only install when otel feature means we skipped the early fmt-only init
    // for the HTTP path — see serve() which calls this before listening.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(fmt_layer)
        .try_init();

    Ok(provider)
}
