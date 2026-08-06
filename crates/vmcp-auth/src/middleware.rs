//! Bearer / forward-auth middleware for protecting `/mcp`.
//!
//! Delegates credential resolution to [`AuthFacade`] so local OAuth and
//! Authentik share one gate. Scope enforcement still happens downstream on
//! every tool call via [`crate::scopes::ScopePolicy`].

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::facade::{AuthFacade, AuthReject};
use crate::types::AccessTokenClaims;

/// Reject if authentication fails. On success, attach verified claims so
/// downstream handlers can introspect them.
pub async fn require_bearer(
    State(facade): State<AuthFacade>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    match facade.authenticate(req.headers()).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity.into_claims());
            next.run(req).await
        }
        Err(reject) => unauthorized(&facade, reject),
    }
}

fn unauthorized(facade: &AuthFacade, reject: AuthReject) -> Response {
    let error = reject.as_error_code();
    let challenge = facade.www_authenticate(error);
    let status = match reject {
        AuthReject::InsufficientScope => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    };
    let mut resp = (status, error.to_string()).into_response();
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
    use crate::providers::local::LocalAuth;
    use crate::state::AuthState;
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

    fn facade_with_store(file: &Path) -> AuthFacade {
        let jwks = JwksManager::new_with_fresh("kid-test").unwrap();
        let store = StaticTokenStore::load(file).unwrap();
        let state = AuthState::new(jwks, "https://iss", "https://iss", 3600, DUMMY_HASH)
            .with_token_store(store);
        AuthFacade::Local(LocalAuth::new(state))
    }

    async fn echo_client_id(req: Request<Body>) -> Response {
        match claims_from_extensions(req.extensions()) {
            Some(c) => (StatusCode::OK, c.client_id.clone()).into_response(),
            None => (StatusCode::OK, "no-claims".to_string()).into_response(),
        }
    }

    fn app(facade: AuthFacade) -> Router {
        Router::new()
            .route("/mcp", post(echo_client_id))
            .layer(axum::middleware::from_fn_with_state(facade, require_bearer))
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

        let resp = app(facade_with_store(&file))
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
        let entry = static_tokens::generate_entry("ci", None).unwrap();
        static_tokens::append_atomic(&file, &entry).unwrap();

        let resp = app(facade_with_store(&file))
            .oneshot(bearer_req("vmcp_definitely-not-registered"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn jwt_path_still_works_when_store_present() {
        let dir = TempDir::new();
        let file = dir.path().join("tokens.json");
        let facade = facade_with_store(&file);
        let AuthFacade::Local(local) = &facade else {
            panic!("expected local");
        };
        let (jwt, _) = issue_access_token(
            &local.state.jwks,
            &local.state.issuer,
            &local.state.resource_audience,
            "jwt-client",
            "mcp:use",
            3600,
        )
        .unwrap();

        let resp = app(facade).oneshot(bearer_req(&jwt)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "jwt-client");
    }

    fn admin_app(facade: AuthFacade) -> Router {
        Router::new()
            .route("/api/v1/ping", post(echo_client_id))
            .layer(axum::middleware::from_fn(require_admin_scope))
            .layer(axum::middleware::from_fn_with_state(facade, require_bearer))
    }

    fn admin_bearer_req(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/ping")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn require_admin_scope_allows_mcp_admin_rejects_mcp_use() {
        let dir = TempDir::new();
        let file = dir.path().join("tokens.json");
        let admin = static_tokens::generate_entry("op", Some(static_tokens::SCOPE_ADMIN)).unwrap();
        let agent = static_tokens::generate_entry("agent", Some("mcp:use")).unwrap();
        static_tokens::append_atomic(&file, &admin).unwrap();
        static_tokens::append_atomic(&file, &agent).unwrap();
        let facade = facade_with_store(&file);

        let ok = admin_app(facade.clone())
            .oneshot(admin_bearer_req(&admin.token))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let forbidden = admin_app(facade.clone())
            .oneshot(admin_bearer_req(&agent.token))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let missing = admin_app(facade)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ping")
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    }
}
