//! Auth facade: one gate for local OAuth and external Authentik.
//!
//! Browser traffic (forward-auth) and machine MCP clients (Bearer JWT) are
//! different clients — this facade is the single place that turns either into
//! a normalized [`AuthIdentity`] on **every** request. Downstream scope checks
//! always see a resolved identity; there is no anonymous / default role.

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
        AccessTokenClaims {
            iss: self.issuer,
            aud: self.audience,
            sub: self.subject,
            client_id: self.client_id,
            scope: self.scope,
            iat: self.iat,
            exp: self.exp,
            jti: self.jti,
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
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthIdentity, AuthReject> {
        match self {
            Self::Local(p) => p.authenticate(headers),
            Self::Authentik(p) => p.authenticate(headers).await,
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
