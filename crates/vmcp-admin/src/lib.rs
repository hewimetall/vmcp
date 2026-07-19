//! vmcp operator admin panel.
//!
//! Mounted under `/admin` by the bin crate. HTTP Basic auth against the same
//! argon2id master-password hash that gates `/consent`. Per-IP rate limiter
//! drops brute-forcers to 429 after a small window of failures.
//!
//! The UI is a single four-zone SPA (`templates/main.html` +
//! `static/admin.{css,js}`). CSP is tightened so no inline scripts are
//! required — every page sources its JS from `static/`.

#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_graphql::dynamic::Schema;
use axum::Router;

use vmcp_auth::AuthState;
use vmcp_notify::Bus;
use vmcp_server::sessions::SessionRegistry;
use vmcp_server::Skill;
use vmcp_upstream::UpstreamPool;

#[cfg(feature = "otel")]
use vmcp_server::otel_file::SpanStore as DumpStore;
#[cfg(not(feature = "otel"))]
use vmcp_server::recorder::Recorder as DumpStore;

mod api;
mod auth;
mod pages;
mod rate_limit;
mod security;

#[cfg(test)]
mod integration;
#[cfg(test)]
mod ui_regression;

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
    /// Per-session dump store (`Recorder` or OTEL [`SpanStore`](vmcp_server::otel_file::SpanStore)).
    pub recorder: Arc<DumpStore>,
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
        recorder: Arc<DumpStore>,
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
    let static_dir = resolve_static_dir();

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

fn resolve_static_dir() -> PathBuf {
    std::env::var("VMCP_ADMIN_STATIC_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            ["crates/vmcp-admin/static", "static"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"))
}

#[cfg(test)]
mod resolve_static_tests {
    use super::resolve_static_dir;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_static_dir_uses_env_when_set() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("vmcp-admin-static-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("VMCP_ADMIN_STATIC_DIR", &dir);
        let got = resolve_static_dir();
        std::env::remove_var("VMCP_ADMIN_STATIC_DIR");
        assert_eq!(got, dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_static_dir_finds_crate_static() {
        std::env::remove_var("VMCP_ADMIN_STATIC_DIR");
        // Point cwd-relative candidates away from a hit so we exercise the
        // CARGO_MANIFEST_DIR fallback (always present in this crate).
        let got = resolve_static_dir();
        assert!(
            got.join("admin.css").is_file() || got.ends_with("static"),
            "unexpected static dir {got:?}"
        );
    }
}
