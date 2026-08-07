//! Auth facade: one gate for local OAuth and external Authentik.
//!
//! Browser traffic (forward-auth) and machine MCP clients (Bearer JWT) are
//! different clients — this facade is the single place that turns either into
//! a normalized [`AuthIdentity`] on **every** request. Downstream scope checks
//! always see a resolved identity; there is no anonymous / default role.

use std::net::IpAddr;

use axum::http::{header, HeaderMap};

use crate::providers::authentik::AuthentikAuth;
use crate::providers::local::LocalAuth;
use crate::types::AccessTokenClaims;

/// How the identity was established (audit / debugging).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    /// Local AS JWT (vmcp-issued RS256).
    LocalJwt,
    /// Pre-registered opaque `vmcp_…` token.
    StaticToken,
    /// Authentik-issued Bearer JWT verified against remote JWKS.
    AuthentikJwt,
    /// Trusted gateway forward-auth headers (`X-authentik-*`).
    AuthentikForwardAuth,
}

/// Normalized identity after a successful authenticate call.
#[derive(Debug, Clone)]
pub struct AuthIdentity {
    pub subject: String,
    pub client_id: String,
    pub scope: String,
    pub groups: Vec<String>,
    pub issuer: String,
    pub audience: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub source: AuthSource,
}

impl AuthIdentity {
    /// Project into the claims type already consumed by MCP / GraphQL / admin.
    pub fn into_claims(self) -> AccessTokenClaims {
        AccessTokenClaims::from(self)
    }
}

impl From<AuthIdentity> for AccessTokenClaims {
    fn from(id: AuthIdentity) -> Self {
        Self {
            iss: id.issuer,
            aud: id.audience,
            sub: id.subject,
            client_id: id.client_id,
            scope: id.scope,
            iat: id.iat,
            exp: id.exp,
            jti: id.jti,
        }
    }
}

/// Why authentication failed. Mapped to WWW-Authenticate `error=…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthReject {
    MissingBearer,
    EmptyBearer,
    InvalidToken,
    /// Forward-auth: required Authentik header absent — never invent identity.
    MissingForwardAuthHeader(&'static str),
    /// Forward-auth enabled but no trusted hop configured (fail closed).
    UntrustedForwardAuth,
    /// TCP peer is not in `trusted_proxies`.
    UntrustedForwardAuthPeer,
    /// Shared hop secret missing or mismatched.
    InvalidForwardAuthSecret,
    /// Authenticated but no MCP scope after group mapping.
    InsufficientScope,
}

impl AuthReject {
    pub fn as_error_code(&self) -> &'static str {
        match self {
            Self::MissingBearer => "missing_bearer",
            Self::EmptyBearer => "empty_bearer",
            Self::InvalidToken => "invalid_token",
            Self::MissingForwardAuthHeader(_) => "missing_forward_auth",
            Self::UntrustedForwardAuth => "untrusted_forward_auth",
            Self::UntrustedForwardAuthPeer => "untrusted_forward_auth",
            Self::InvalidForwardAuthSecret => "untrusted_forward_auth",
            Self::InsufficientScope => "insufficient_scope",
        }
    }
}

/// Facade over local OAuth AS/RS and Authentik (JWT + forward-auth).
///
/// Clone is cheap (`Arc` inside each variant). Middleware holds this as state.
#[derive(Clone)]
pub enum AuthFacade {
    Local(LocalAuth),
    Authentik(AuthentikAuth),
}

impl AuthFacade {
    /// Authenticate **this** request. Must be called on every protected request;
    /// never cache a default role when credentials/headers are omitted.
    ///
    /// `peer` is the TCP peer address (`ConnectInfo`); required when Authentik
    /// forward-auth uses `trusted_proxies`.
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
    ) -> Result<AuthIdentity, AuthReject> {
        match self {
            Self::Local(p) => p.authenticate(headers),
            Self::Authentik(p) => p.authenticate(headers, peer).await,
        }
    }

    /// Issuer(s) advertised in Protected Resource Metadata `authorization_servers`.
    pub fn authorization_servers(&self) -> Vec<String> {
        match self {
            Self::Local(p) => vec![p.state.issuer.clone()],
            Self::Authentik(p) => vec![p.issuer.clone()],
        }
    }

    /// Primary resource indicator for bare PRM + WWW-Authenticate.
    pub fn resource(&self) -> &str {
        match self {
            Self::Local(p) => p.state.resource_audience.as_str(),
            Self::Authentik(p) => p.resource.as_str(),
        }
    }

    /// All accepted resource audiences (`/mcp`, `/mcp-proxy`, …).
    pub fn resource_audiences(&self) -> &[String] {
        match self {
            Self::Local(p) => &p.state.resource_audiences,
            Self::Authentik(p) => &p.audiences,
        }
    }

    /// Whether vmcp should mount the local OAuth AS (`/authorize`, DCR, …).
    pub fn serves_local_as(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// `WWW-Authenticate` challenge for 401 responses.
    pub fn www_authenticate(&self, error: &str) -> String {
        let base = match self {
            Self::Local(p) => p.state.issuer.trim_end_matches('/').to_string(),
            Self::Authentik(p) => p.resource.trim_end_matches('/').to_string(),
        };
        let prm = format!("{base}/.well-known/oauth-protected-resource");
        format!("Bearer resource_metadata=\"{prm}\", error=\"{error}\"")
    }

    /// Extract Bearer raw token if present.
    pub(crate) fn bearer_token(headers: &HeaderMap) -> Result<Option<&str>, AuthReject> {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(None);
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return Ok(None);
        };
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthReject::EmptyBearer);
        }
        Ok(Some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwks::JwksManager;
    use crate::providers::authentik::{AuthentikAuth, AuthentikConfig};
    use crate::providers::local::LocalAuth;
    use crate::state::AuthState;
    use axum::http::HeaderValue;
    use std::collections::BTreeMap;

    const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4";

    fn local_facade() -> AuthFacade {
        let jwks = JwksManager::new_with_fresh("facade").unwrap();
        let state = AuthState::new(jwks, "https://iss", "https://iss/mcp", 3600, DUMMY_HASH);
        AuthFacade::Local(LocalAuth::new(state))
    }

    fn authentik_facade() -> AuthFacade {
        let mut group_scopes = BTreeMap::new();
        group_scopes.insert("mcp-users".into(), "mcp:use".into());
        AuthFacade::Authentik(
            AuthentikAuth::new(AuthentikConfig {
                issuer: "https://auth.example/application/o/mcp/".into(),
                jwks_url: "https://auth.example/application/o/mcp/jwks/".into(),
                audiences: vec!["https://mcp.example/mcp".into()],
                resource: "https://mcp.example/mcp".into(),
                accept_bearer: false,
                forward_auth: true,
                trusted_proxies: vec!["127.0.0.1/32".into()],
                group_scopes,
                ..Default::default()
            })
            .unwrap(),
        )
    }

    #[test]
    fn reject_codes_cover_all_variants() {
        assert_eq!(AuthReject::MissingBearer.as_error_code(), "missing_bearer");
        assert_eq!(AuthReject::EmptyBearer.as_error_code(), "empty_bearer");
        assert_eq!(AuthReject::InvalidToken.as_error_code(), "invalid_token");
        assert_eq!(
            AuthReject::MissingForwardAuthHeader("x").as_error_code(),
            "missing_forward_auth"
        );
        assert_eq!(
            AuthReject::InsufficientScope.as_error_code(),
            "insufficient_scope"
        );
        assert_eq!(
            AuthReject::UntrustedForwardAuthPeer.as_error_code(),
            "untrusted_forward_auth"
        );
        assert_eq!(
            AuthReject::InvalidForwardAuthSecret.as_error_code(),
            "untrusted_forward_auth"
        );
    }

    #[test]
    fn into_claims_preserves_identity_fields() {
        let id = AuthIdentity {
            subject: "u".into(),
            client_id: "c".into(),
            scope: "mcp:use".into(),
            groups: vec!["g".into()],
            issuer: "iss".into(),
            audience: "aud".into(),
            iat: 1,
            exp: 2,
            jti: "j".into(),
            source: AuthSource::LocalJwt,
        };
        let c = id.into_claims();
        assert_eq!(c.sub, "u");
        assert_eq!(c.client_id, "c");
        assert_eq!(c.scope, "mcp:use");
        assert_eq!(c.aud, "aud");
    }

    #[test]
    fn local_facade_metadata_helpers() {
        let f = local_facade();
        assert!(f.serves_local_as());
        assert_eq!(f.resource(), "https://iss/mcp");
        assert_eq!(f.authorization_servers(), vec!["https://iss".to_string()]);
        assert_eq!(f.resource_audiences(), &["https://iss/mcp".to_string()]);
        let challenge = f.www_authenticate("missing_bearer");
        assert!(challenge.contains("resource_metadata="));
        assert!(challenge.contains("missing_bearer"));
    }

    #[tokio::test]
    async fn authentik_facade_metadata_and_forward_auth() {
        let f = authentik_facade();
        assert!(!f.serves_local_as());
        assert_eq!(f.resource(), "https://mcp.example/mcp");
        assert_eq!(
            f.authorization_servers(),
            vec!["https://auth.example/application/o/mcp/".to_string()]
        );
        assert!(f
            .www_authenticate("x")
            .contains("https://mcp.example/mcp/.well-known/oauth-protected-resource"));

        let mut headers = HeaderMap::new();
        headers.insert("x-authentik-username", HeaderValue::from_static("alice"));
        headers.insert("x-authentik-groups", HeaderValue::from_static("mcp-users"));
        let id = f
            .authenticate(
                &headers,
                Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            )
            .await
            .unwrap();
        assert_eq!(id.source, AuthSource::AuthentikForwardAuth);
        assert_eq!(id.scope, "mcp:use");

        // Forged headers from an untrusted peer must fail.
        let err = f
            .authenticate(
                &headers,
                Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8))),
            )
            .await
            .unwrap_err();
        assert_eq!(err, AuthReject::UntrustedForwardAuthPeer);
    }

    #[test]
    fn bearer_token_empty_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer   "));
        assert_eq!(
            AuthFacade::bearer_token(&headers).unwrap_err(),
            AuthReject::EmptyBearer
        );
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Token abc"));
        assert_eq!(AuthFacade::bearer_token(&headers).unwrap(), None);
        assert_eq!(AuthFacade::bearer_token(&HeaderMap::new()).unwrap(), None);
    }
}
