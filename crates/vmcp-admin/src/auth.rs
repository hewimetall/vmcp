//! HTTP Basic auth middleware against the master password hash.
//!
//! Username is ignored — only the password is checked. Mirrors Python admin's
//! UX: browser shows the native prompt, no login form, no cookie session,
//! no CSRF surface. Per-IP rate limiter blocks brute-force at 429.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;

use crate::AdminState;

/// Marker that an auth check succeeded (cleared the rate limiter etc).
#[derive(Clone, Copy, Debug)]
pub struct AdminAuth;

pub async fn require_basic_auth(
    State(state): State<AdminState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Prefer the real peer address from the listener; fall back to loopback
    // when ConnectInfo is absent (unit/integration tests via oneshot).
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    if state.rate_limiter.is_blocked(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "rate limited",
        )
            .into_response();
    }

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let password = match auth_header.as_deref().and_then(decode_basic) {
        Some(p) => p,
        None => return unauthorized_response(),
    };

    let ok = vmcp_auth::password::verify_master(&password, &state.master_hash).unwrap_or(false);

    if !ok {
        state.rate_limiter.record_failure(ip);
        return unauthorized_response();
    }

    req.extensions_mut().insert(AdminAuth);
    next.run(req).await
}

/// Parse `Authorization: Basic base64(user:pass)`. Returns the password
/// portion (we ignore the username — single-tenant admin).
fn decode_basic(header_value: &str) -> Option<String> {
    let rest = header_value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    let (_user, pass) = s.split_once(':')?;
    Some(pass.to_string())
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"vmcp admin\", charset=\"UTF-8\"",
        )],
        "unauthorized",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basic_extracts_password() {
        // base64("admin:hunter2") = "YWRtaW46aHVudGVyMg=="
        assert_eq!(
            decode_basic("Basic YWRtaW46aHVudGVyMg=="),
            Some("hunter2".into())
        );
    }

    #[test]
    fn decode_basic_rejects_non_basic() {
        assert!(decode_basic("Bearer abc").is_none());
    }

    #[test]
    fn decode_basic_handles_password_with_colons() {
        // base64("u:a:b:c") = "dTphOmI6Yw=="
        assert_eq!(decode_basic("Basic dTphOmI6Yw=="), Some("a:b:c".into()));
    }
}
