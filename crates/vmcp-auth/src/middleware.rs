//! Bearer-auth middleware for protecting `/mcp`.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AuthState;
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

    match verify_access_token(&state.jwks, token, &state.issuer, &state.resource_audience) {
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

fn unauthorized(state: &AuthState, error: &str) -> Response {
    let prm = format!(
        "{}/.well-known/oauth-protected-resource",
        state.issuer.trim_end_matches('/')
    );
    let challenge =
        format!("Bearer resource_metadata=\"{prm}\", error=\"{error}\"");
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
