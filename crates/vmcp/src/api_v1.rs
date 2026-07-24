//! Operator control-plane JSON API at `/api/v1`.
//!
//! Authenticated with Bearer + scope `mcp:admin` (not HTTP Basic — that stays
//! on `/admin`). Token CRUD writes the same `tokens_file` format as
//! `vmcp pre-reg`; the existing file watcher hot-reloads the in-memory store.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use vmcp_auth::static_tokens::{
    self, generate_entry, read_entries, revoke_by_client_id, RevokeOutcome, SCOPE_ADMIN,
};
use vmcp_auth::AuthState;

/// Shared state for `/api/v1` handlers.
#[derive(Clone)]
pub struct ApiV1State {
    pub auth: AuthState,
    pub tokens_file: Option<PathBuf>,
    pub tokens_write_lock: Arc<Mutex<()>>,
    /// Optional registry reconcile hook (Phase 2). `None` → upstreams reload 503.
    pub reload_registry: Option<RegistryReloader>,
}

/// Async callback that reconciles `registry.json` into the live pool/schema.
pub type RegistryReloader = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send>,
        > + Send
        + Sync,
>;

impl ApiV1State {
    pub fn new(auth: AuthState, tokens_file: Option<PathBuf>) -> Self {
        Self {
            auth,
            tokens_file,
            tokens_write_lock: Arc::new(Mutex::new(())),
            reload_registry: None,
        }
    }

    pub fn with_reload(mut self, reload: RegistryReloader) -> Self {
        self.reload_registry = Some(reload);
        self
    }
}

/// Router nested at `/api/v1`. Caller must layer bearer + admin-scope middleware
/// with `AuthState` (see `main::serve_http`).
pub fn router(state: ApiV1State) -> Router {
    Router::new()
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/:client_id", axum::routing::delete(delete_token))
        .route("/upstreams", get(list_upstreams))
        .route("/upstreams/reload", post(reload_upstreams))
        .with_state(state)
}

async fn list_tokens(State(s): State<ApiV1State>) -> impl IntoResponse {
    let path = match tokens_path(&s) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match read_entries(&path) {
        Ok(entries) => {
            let items: Vec<Value> = entries
                .iter()
                .map(|e| serde_json::to_value(e.to_list_item()).unwrap_or(json!({})))
                .collect();
            (StatusCode::OK, Json(json!({ "tokens": items }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct CreateTokenBody {
    name: String,
    #[serde(default)]
    scope: Option<String>,
}

async fn create_token(
    State(s): State<ApiV1State>,
    Json(body): Json<CreateTokenBody>,
) -> impl IntoResponse {
    let path = match tokens_path(&s) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let _guard = s.tokens_write_lock.lock().await;

    let existing = match read_entries(&path) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if existing.iter().any(|e| e.client_id == body.name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("client_id already exists: {}", body.name) })),
        )
            .into_response();
    }

    let entry = match generate_entry(&body.name, body.scope.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if let Err(e) = static_tokens::append_atomic(&path, &entry) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // Best-effort: if a store is attached, reload immediately so the new token
    // works even if the file watcher is slow/unavailable.
    if let Some(store) = s.auth.token_store.as_ref() {
        store.reload(&path);
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": entry.client_id,
            "name": entry.name,
            "scope": entry.scope,
            "issued_at": entry.issued_at,
            "token": entry.token,
        })),
    )
        .into_response()
}

async fn delete_token(
    State(s): State<ApiV1State>,
    Path(client_id): Path<String>,
) -> impl IntoResponse {
    let path = match tokens_path(&s) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if !static_tokens::valid_id(&client_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid client_id" })),
        )
            .into_response();
    }
    let _guard = s.tokens_write_lock.lock().await;
    match revoke_by_client_id(&path, &client_id) {
        Ok(RevokeOutcome::Revoked) => {
            if let Some(store) = s.auth.token_store.as_ref() {
                store.reload(&path);
            }
            (StatusCode::NO_CONTENT, ()).into_response()
        }
        Ok(RevokeOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown client_id: {client_id}") })),
        )
            .into_response(),
        Ok(RevokeOutcome::LastAdmin) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "refusing to revoke last token with scope {SCOPE_ADMIN}"
                )
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_upstreams(State(s): State<ApiV1State>) -> impl IntoResponse {
    // Phase 2 fills this via reload handle / pool snapshot. Until then, point
    // operators at reload or return empty when no reloader is wired.
    if s.reload_registry.is_none() {
        return (
            StatusCode::OK,
            Json(json!({
                "upstreams": [],
                "note": "registry reload not wired; POST /api/v1/upstreams/reload unavailable"
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "upstreams": [],
            "note": "use POST /api/v1/upstreams/reload; detailed status lands with pool snapshot"
        })),
    )
        .into_response()
}

async fn reload_upstreams(State(s): State<ApiV1State>) -> impl IntoResponse {
    let Some(reload) = s.reload_registry.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "registry reload not configured" })),
        )
            .into_response();
    };
    match reload().await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn tokens_path(s: &ApiV1State) -> Result<PathBuf, axum::response::Response> {
    match &s.tokens_file {
        Some(p) => Ok(p.clone()),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "auth.tokens_file is not configured"
            })),
        )
            .into_response()),
    }
}
