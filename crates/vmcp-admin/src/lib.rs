//! vmcp operator admin panel.
//!
//! Mounted under `/admin` by the bin crate. HTTP Basic auth against the same
//! argon2id master-password hash that gates `/consent`. Per-IP rate limiter
//! drops brute-forcers to 429 after a small window of failures.
//!
//! Templates use `askama`. CSS/JS load from jsDelivr CDN (Tabler 1.x,
//! Grid.js 6.x) plus locally-served files under `/admin/static/*`. CSP is
//! tightened so no inline scripts are required — every page sources its JS
//! from `static/`.

#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_graphql::dynamic::Schema;
use axum::Router;

use vmcp_auth::AuthState;
use vmcp_notify::Bus;
use vmcp_server::recorder::Recorder;
use vmcp_server::sessions::SessionRegistry;
use vmcp_server::Skill;
use vmcp_upstream::UpstreamPool;

mod api;
mod auth;
mod pages;
mod rate_limit;
mod security;

pub use auth::AdminAuth;
pub use rate_limit::RateLimiter;

/// Shared state for every admin route. Cheap to clone (everything is `Arc`).
#[derive(Clone)]
pub struct AdminState {
    pub pool: Arc<UpstreamPool>,
    pub schema: Arc<ArcSwap<Schema>>,
    pub bus: Arc<Bus>,
    /// Hot-swappable skills handle — shared with the MCP server. CRUD writes
    /// the YAML file, reloads from disk, and `.store()`s the new pointee.
    pub skills: Arc<ArcSwap<Vec<Skill>>>,
    /// Directory the skill YAMLs live in.
    pub skills_dir: PathBuf,
    /// Serializes concurrent admin CRUD writes against `skills_dir`. Read
    /// paths (`list_skills`, MCP `prompts/*`) never take this lock — they
    /// snapshot through the `ArcSwap`.
    pub skills_write_lock: Arc<tokio::sync::Mutex<()>>,
    /// PHC-encoded argon2id master password hash.
    pub master_hash: Arc<String>,
    pub rate_limiter: Arc<RateLimiter>,
    /// OAuth/DCR registered clients — source of `pre_registered` state in
    /// the Sessions aggregation.
    pub auth_state: AuthState,
    /// Live MCP session registry filled by the `/mcp` capture middleware.
    pub registry: Arc<SessionRegistry>,
    /// Recorder for MCP wire dumps (per-session JSONL on disk + SSE fan-out).
    pub recorder: Arc<Recorder>,
}

impl AdminState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Arc<UpstreamPool>,
        schema: Arc<ArcSwap<Schema>>,
        bus: Arc<Bus>,
        skills: Arc<ArcSwap<Vec<Skill>>>,
        skills_dir: PathBuf,
        master_hash: String,
        auth_state: AuthState,
        registry: Arc<SessionRegistry>,
        recorder: Arc<Recorder>,
    ) -> Self {
        Self {
            pool,
            schema,
            bus,
            skills,
            skills_dir,
            skills_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            master_hash: Arc::new(master_hash),
            rate_limiter: Arc::new(RateLimiter::new(10, std::time::Duration::from_secs(60))),
            auth_state,
            registry,
            recorder,
        }
    }
}

/// Mount all `/admin/*` routes onto a new router. The bin nests this under
/// `/admin` via `.nest("/admin", vmcp_admin::router(state))`. Every route is
/// fronted by HTTP Basic + rate limiter + security headers.
pub fn router(state: AdminState) -> Router {
    // Resolve the static-asset directory. Works whether the binary is launched
    // from the workspace root (cargo run) or from `/usr/local/bin` in a
    // container with arbitrary cwd. Tries, in order:
    //   1. $VMCP_ADMIN_STATIC_DIR      (explicit override — used in Docker)
    //   2. ./crates/vmcp-admin/static  (cargo run from workspace root)
    //   3. ./static                    (working dir already inside the crate)
    //   4. CARGO_MANIFEST_DIR/static   (compile-time fallback)
    let static_dir = std::env::var("VMCP_ADMIN_STATIC_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            ["crates/vmcp-admin/static", "static"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));

    // Layer order in axum: last `.layer()` is the OUTERMOST. Security headers
    // sit outside auth so they apply even to 401 responses. Auth sits outside
    // the handlers so it short-circuits unauthorized requests.
    Router::new()
        .merge(pages::routes())
        .merge(api::routes())
        .nest_service("/static", tower_http::services::ServeDir::new(static_dir))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_basic_auth,
        ))
        .layer(axum::middleware::from_fn(security::headers))
        .with_state(state)
}
