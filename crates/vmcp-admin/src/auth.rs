//! `/admin` auth facade: `none` | HTTP Basic (`user:pass`) | Authentik headers.
//!
//! - **none** — open (local/dev only); never invent a fake identity, just skip the gate.
//! - **basic** — `Authorization: Basic` against `master_password_argon2`.
//! - **authentik** — trust `X-authentik-username` / `X-authentik-groups` from the
//!   gateway; exact group membership after delimiter split; missing headers → 401.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderName, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use vmcp_auth::ForwardAuthTrust;

use crate::AdminState;

/// How the admin identity was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminAuthSource {
    None,
    Basic,
    AuthentikHeaders,
}

/// Marker (+ subject) that an auth check succeeded.
#[derive(Clone, Debug)]
pub struct AdminAuth {
    pub subject: String,
    pub source: AdminAuthSource,
}

/// Runtime policy for the `/admin` gate (from `[auth.admin]`).
#[derive(Clone, Debug, Default)]
pub enum AdminAuthPolicy {
    /// No authentication.
    None,
    /// HTTP Basic against the master password hash on [`AdminState`].
    #[default]
    Basic,
    /// Authentik forward-auth headers (same hop trust as MCP facade).
    Authentik {
        username_header: HeaderName,
        groups_header: HeaderName,
        /// Exact group names (any one is enough).
        required_groups: Vec<String>,
        /// Gate `X-authentik-*` to trusted proxies / hop secret.
        trust: ForwardAuthTrust,
    },
}

pub async fn require_admin_auth(
    State(state): State<AdminState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    match &state.admin_auth {
        AdminAuthPolicy::None => {
            req.extensions_mut().insert(AdminAuth {
                subject: "anonymous".into(),
                source: AdminAuthSource::None,
            });
            next.run(req).await
        }
        AdminAuthPolicy::Basic => {
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

            let (user, password) = match auth_header.as_deref().and_then(decode_basic) {
                Some(p) => p,
                None => return unauthorized_basic(),
            };

            let ok =
                vmcp_auth::password::verify_master(&password, &state.master_hash).unwrap_or(false);

            if !ok {
                state.rate_limiter.record_failure(ip);
                return unauthorized_basic();
            }

            let subject = if user.is_empty() {
                "admin".into()
            } else {
                user
            };
            req.extensions_mut().insert(AdminAuth {
                subject,
                source: AdminAuthSource::Basic,
            });
            next.run(req).await
        }
        AdminAuthPolicy::Authentik {
            username_header,
            groups_header,
            required_groups,
            trust,
        } => {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());
            match authenticate_authentik_headers(
                req.headers(),
                peer,
                username_header,
                groups_header,
                required_groups,
                trust,
            ) {
                Ok(subject) => {
                    req.extensions_mut().insert(AdminAuth {
                        subject,
                        source: AdminAuthSource::AuthentikHeaders,
                    });
                    next.run(req).await
                }
                Err(resp) => resp,
            }
        }
    }
}

fn authenticate_authentik_headers(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    username_header: &HeaderName,
    groups_header: &HeaderName,
    required_groups: &[String],
    trust: &ForwardAuthTrust,
) -> Result<String, Response> {
    if let Err(reject) = trust.verify(headers, peer) {
        return Err(unauthorized_forward(reject.as_error_code()));
    }

    let username = headers
        .get(username_header)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| unauthorized_forward("missing X-authentik-username"))?;

    let groups_raw = headers
        .get(groups_header)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let groups = vmcp_auth::split_groups(groups_raw);

    let allowed = required_groups
        .iter()
        .any(|need| vmcp_auth::group_contains(&groups, need));
    if !allowed {
        return Err(unauthorized_forward("insufficient_group"));
    }
    Ok(username.to_string())
}

/// Parse `Authorization: Basic base64(user:pass)`.
/// Returns `(username, password)`. Username may be empty.
fn decode_basic(header_value: &str) -> Option<(String, String)> {
    let rest = header_value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn unauthorized_basic() -> Response {
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

fn unauthorized_forward(reason: &'static str) -> Response {
    (StatusCode::UNAUTHORIZED, reason).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn decode_basic_extracts_user_and_password() {
        // base64("admin:hunter2") = "YWRtaW46aHVudGVyMg=="
        assert_eq!(
            decode_basic("Basic YWRtaW46aHVudGVyMg=="),
            Some(("admin".into(), "hunter2".into()))
        );
    }

    #[test]
    fn decode_basic_rejects_non_basic() {
        assert!(decode_basic("Bearer abc").is_none());
    }

    #[test]
    fn decode_basic_handles_password_with_colons() {
        // base64("u:a:b:c") = "dTphOmI6Yw=="
        assert_eq!(
            decode_basic("Basic dTphOmI6Yw=="),
            Some(("u".into(), "a:b:c".into()))
        );
    }

    #[test]
    fn authentik_headers_require_username_and_exact_group() {
        let user_h = HeaderName::from_static("x-authentik-username");
        let groups_h = HeaderName::from_static("x-authentik-groups");
        let required = vec!["mcp-admins".into()];
        let trust =
            ForwardAuthTrust::new(&["127.0.0.1/32".into()], "", "x-vmcp-forward-auth").unwrap();
        let peer = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

        let mut headers = HeaderMap::new();
        let err =
            authenticate_authentik_headers(&headers, peer, &user_h, &groups_h, &required, &trust);
        assert!(err.is_err());

        headers.insert(&user_h, HeaderValue::from_static("alice"));
        headers.insert(&groups_h, HeaderValue::from_static("mcp-admins-extra|ops"));
        // mcp-admins-extra must NOT satisfy mcp-admins
        assert!(authenticate_authentik_headers(
            &headers, peer, &user_h, &groups_h, &required, &trust
        )
        .is_err());

        headers.insert(&groups_h, HeaderValue::from_static("ops|mcp-admins"));
        let subject =
            authenticate_authentik_headers(&headers, peer, &user_h, &groups_h, &required, &trust)
                .unwrap();
        assert_eq!(subject, "alice");

        // Untrusted peer cannot forge admin headers.
        assert!(authenticate_authentik_headers(
            &headers,
            Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))),
            &user_h,
            &groups_h,
            &required,
            &trust,
        )
        .is_err());
    }
}
