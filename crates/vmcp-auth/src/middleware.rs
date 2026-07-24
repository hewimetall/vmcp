//! Bearer-auth middleware for protecting `/mcp`.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AuthState;
use crate::static_tokens::{self, TokenInfo};
use crate::tokens::verify_access_token;
use crate::types::AccessTokenClaims;

/// Reject if no valid Bearer token. On success, attach the verified claims to
/// the request extensions so downstream handlers can introspect them.
pub async fn require_bearer(
    State(state): State<AuthState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match header_value.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t.trim(),
        None => return unauthorized(&state, "missing_bearer"),
    };
    if token.is_empty() {
        return unauthorized(&state, "empty_bearer");
    }

    // Static "pre-registered" token fast-path: opaque, eternal, file-backed,
    // independent of JWKS. A `vmcp_`-prefixed bearer is NEVER a JWT, so a miss
    // 401s immediately rather than falling through to (futile) JWT verify.
    // Non-prefixed tokens skip this block and take the JWT path unchanged.
    if let Some(store) = &state.token_store {
        if token.starts_with(static_tokens::TOKEN_PREFIX) {
            return match store.lookup(token) {
                Some(info) => {
                    req.extensions_mut()
                        .insert(synth_static_claims(&state, &info));
                    next.run(req).await
                }
                None => {
                    tracing::debug!("unknown static token");
                    unauthorized(&state, "invalid_token")
                }
            };
        }
    }

    let audiences = state.audience_refs();
    match verify_access_token(&state.jwks, token, &state.issuer, &audiences) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(e) => {
            tracing::debug!(error = %e, "bearer rejected");
            unauthorized(&state, "invalid_token")
        }
    }
}

/// Build claims for a verified static token. Mirrors the JWT claim shape so
/// everything downstream (recorder, admin) treats it uniformly. `exp` is set
/// ~100 years out — far-future but overflow-safe (not `i64::MAX`); nothing
/// re-validates it after this point, since the static path never expires.
fn synth_static_claims(state: &AuthState, info: &TokenInfo) -> AccessTokenClaims {
    let now = chrono::Utc::now().timestamp();
    let exp = now + 100 * 365 * 24 * 3600;
    AccessTokenClaims {
        iss: state.issuer.clone(),
        aud: state.resource_audience.clone(),
        sub: info.client_id.clone(),
        client_id: info.client_id.clone(),
        scope: info.scope.clone(),
        iat: now,
        exp,
        jti: uuid::Uuid::new_v4().to_string(),
    }
}

fn unauthorized(state: &AuthState, error: &str) -> Response {
    let prm = format!(
        "{}/.well-known/oauth-protected-resource",
        state.issuer.trim_end_matches('/')
    );
    let challenge = format!("Bearer resource_metadata=\"{prm}\", error=\"{error}\"");
    let mut resp = (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        challenge.parse().expect("static header value"),
    );
    resp
}

/// Extract the verified claims from a request extension. Use in handlers
/// downstream of `require_bearer`.
pub fn claims_from_extensions(ext: &axum::http::Extensions) -> Option<&AccessTokenClaims> {
    ext.get::<AccessTokenClaims>()
}

/// After [`require_bearer`], reject unless claims.scope contains `mcp:admin`.
/// Intended for `/api/v1/*` control-plane routes.
pub async fn require_admin_scope(req: Request<Body>, next: Next) -> Response {
    use crate::static_tokens::{scope_contains, SCOPE_ADMIN};

    match claims_from_extensions(req.extensions()) {
        Some(claims) if scope_contains(&claims.scope, SCOPE_ADMIN) => next.run(req).await,
        Some(_) => (StatusCode::FORBIDDEN, "missing scope mcp:admin").into_response(),
        None => unauthorized_plain("missing_bearer"),
    }
}

fn unauthorized_plain(error: &str) -> Response {
    let challenge = format!("Bearer error=\"{error}\"");
    let mut resp = (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        challenge.parse().expect("static header value"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwks::JwksManager;
    use crate::static_tokens::{self, StaticTokenStore};
    use crate::tokens::issue_access_token;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::post,
        Router,
    };
    use std::path::{Path, PathBuf};
    use tower::ServiceExt;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("vmcp-mw-test-{nanos}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4";

    fn state_with_store(file: &Path) -> AuthState {
        let jwks = JwksManager::new_with_fresh("kid-test").unwrap();
        let store = StaticTokenStore::load(file).unwrap();
        AuthState::new(jwks, "https://iss", "https://iss", 3600, DUMMY_HASH).with_token_store(store)
    }

    /// Handler that echoes the resolved `client_id` so tests can assert which
    /// auth path produced the claims.
    async fn echo_client_id(req: Request<Body>) -> Response {
        match claims_from_extensions(req.extensions()) {
            Some(c) => (StatusCode::OK, c.client_id.clone()).into_response(),
            None => (StatusCode::OK, "no-claims".to_string()).into_response(),
        }
    }

    fn app(state: AuthState) -> Router {
        Router::new()
            .route("/mcp", post(echo_client_id))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn bearer_req(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn static_token_in_store_authorizes_and_synthesizes_claims() {
        let dir = TempDir::new();
        let file = dir.path().join("tokens.json");
        let entry = static_tokens::generate_entry("ci", Some("mcp:use")).unwrap();
        static_tokens::append_atomic(&file, &entry).unwrap();

        let resp = app(state_with_store(&file))
            .oneshot(bearer_req(&entry.token))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_string(resp).await,
            "ci",
            "client_id from the token entry"
        );
    }

    #[tokio::test]
    async fn unknown_static_token_is_rejected_without_jwt_fallthrough() {
        let dir = TempDir::new();
        let file = dir.path().join("tokens.json");
        // Store has one token; we present a different vmcp_ token.
        let entry = static_tokens::generate_entry("ci", None).unwrap();
        static_tokens::append_atomic(&file, &entry).unwrap();

        let resp = app(state_with_store(&file))
            .oneshot(bearer_req("vmcp_definitely-not-registered"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn jwt_path_still_works_when_store_present() {
        let dir = TempDir::new();
        let file = dir.path().join("tokens.json"); // empty store
        let state = state_with_store(&file);
        // A real JWT (no vmcp_ prefix) must bypass the static path entirely.
        let (jwt, _) = issue_access_token(
            &state.jwks,
            &state.issuer,
            &state.resource_audience,
            "jwt-client",
            "mcp:use",
            3600,
        )
        .unwrap();

        let resp = app(state).oneshot(bearer_req(&jwt)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "jwt-client");
    }
}
