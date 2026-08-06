//! Authentik provider via [`async_oidc_jwt_validator`] (JWKS cache + OIDC checks)
//! plus optional forward-auth headers from a trusted gateway.
//!
//! Contract:
//! 1. Trust gateway headers only when present — never invent anonymous / default role.
//! 2. Split groups on `|`, `,`, `;`, space and match **exactly** (not substring).
//! 3. Resolve scopes from groups on **every** request (no omitted-parameter bypass).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_oidc_jwt_validator::{Algorithm, OidcConfig, OidcValidator, Validation};
use axum::http::{HeaderMap, HeaderName};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::facade::{AuthFacade, AuthIdentity, AuthReject, AuthSource};
use crate::groups::{scopes_from_groups, split_groups};

/// Configuration snapshot for Authentik-backed auth.
#[derive(Debug, Clone)]
pub struct AuthentikConfig {
    /// OIDC issuer (e.g. `https://auth.example.com/application/o/mcp-internal/`).
    pub issuer: String,
    /// JWKS URL for signature verification.
    pub jwks_url: String,
    /// Accepted JWT `aud` values / resource indicators.
    pub audiences: Vec<String>,
    /// Canonical resource URL advertised in Protected Resource Metadata.
    pub resource: String,
    /// Accept `Authorization: Bearer` JWTs from Authentik.
    pub accept_bearer: bool,
    /// Accept `X-authentik-*` forward-auth headers from the gateway.
    pub forward_auth: bool,
    /// Header carrying the username (default `x-authentik-username`).
    pub username_header: String,
    /// Header carrying groups (default `x-authentik-groups`).
    pub groups_header: String,
    /// JWT claim name for groups (default `groups`).
    pub groups_claim: String,
    /// Exact group name → space-separated MCP scopes.
    pub group_scopes: BTreeMap<String, String>,
}

impl Default for AuthentikConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            jwks_url: String::new(),
            audiences: Vec::new(),
            resource: String::new(),
            accept_bearer: true,
            forward_auth: true,
            username_header: "x-authentik-username".into(),
            groups_header: "x-authentik-groups".into(),
            groups_claim: "groups".into(),
            group_scopes: BTreeMap::new(),
        }
    }
}

/// Authentik auth provider (OIDC resource server + optional forward-auth).
#[derive(Clone)]
pub struct AuthentikAuth {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub resource: String,
    accept_bearer: bool,
    forward_auth: bool,
    username_header: HeaderName,
    groups_header: HeaderName,
    groups_claim: String,
    group_scopes: BTreeMap<String, String>,
    validator: Arc<OidcValidator>,
}

impl AuthentikAuth {
    pub fn new(cfg: AuthentikConfig) -> anyhow::Result<Self> {
        let username_header = HeaderName::from_bytes(cfg.username_header.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid username_header: {e}"))?;
        let groups_header = HeaderName::from_bytes(cfg.groups_header.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid groups_header: {e}"))?;

        // `client_id` in OidcConfig is the default `aud` for `validate()`;
        // we always use `validate_custom` with resource audiences instead.
        let primary_aud = cfg
            .audiences
            .first()
            .cloned()
            .unwrap_or_else(|| cfg.resource.clone());
        let oidc = OidcConfig::new(cfg.issuer.clone(), primary_aud, cfg.jwks_url);
        let validator = OidcValidator::new(oidc);

        Ok(Self {
            issuer: cfg.issuer,
            audiences: cfg.audiences,
            resource: cfg.resource,
            accept_bearer: cfg.accept_bearer,
            forward_auth: cfg.forward_auth,
            username_header,
            groups_header,
            groups_claim: cfg.groups_claim,
            group_scopes: cfg.group_scopes,
            validator: Arc::new(validator),
        })
    }

    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthIdentity, AuthReject> {
        // Machine clients: Bearer first. Browser form redirects are not viable.
        if self.accept_bearer {
            if let Some(token) = AuthFacade::bearer_token(headers)? {
                return self.authenticate_jwt(token).await;
            }
        }

        if self.forward_auth {
            return self.authenticate_forward_auth(headers);
        }

        Err(AuthReject::MissingBearer)
    }

    fn authenticate_forward_auth(&self, headers: &HeaderMap) -> Result<AuthIdentity, AuthReject> {
        let username = headers
            .get(&self.username_header)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(AuthReject::MissingForwardAuthHeader("X-authentik-username"))?;

        // Absent groups header → empty groups (no default role).
        let groups_raw = headers
            .get(&self.groups_header)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let groups = split_groups(groups_raw);
        let scope = scopes_from_groups(&groups, &self.group_scopes);
        if scope.is_empty() {
            return Err(AuthReject::InsufficientScope);
        }

        let now = Utc::now().timestamp();
        let audience = self
            .audiences
            .first()
            .cloned()
            .unwrap_or_else(|| self.resource.clone());

        Ok(AuthIdentity {
            subject: username.to_string(),
            client_id: username.to_string(),
            scope,
            groups,
            issuer: self.issuer.clone(),
            audience,
            iat: now,
            exp: now + 3600,
            jti: Uuid::new_v4().to_string(),
            source: AuthSource::AuthentikForwardAuth,
        })
    }

    async fn authenticate_jwt(&self, token: &str) -> Result<AuthIdentity, AuthReject> {
        let mut validation = Validation::new(Algorithm::RS256);
        let aud_refs: Vec<&str> = self.audiences.iter().map(String::as_str).collect();
        validation.set_audience(&aud_refs);
        validation.set_issuer(&[&self.issuer]);
        validation.validate_aud = true;
        validation.validate_exp = true;

        let claims: AuthentikClaims = self
            .validator
            .validate_custom(token, &validation)
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "authentik jwt rejected");
                AuthReject::InvalidToken
            })?;

        let groups = self.extract_groups(&claims);
        let mut scope = claims.scope.unwrap_or_default();
        let mapped = scopes_from_groups(&groups, &self.group_scopes);
        if scope.is_empty() {
            scope = mapped;
        } else if !mapped.is_empty() {
            let mut set: std::collections::BTreeSet<String> =
                scope.split_whitespace().map(str::to_string).collect();
            for tok in mapped.split_whitespace() {
                set.insert(tok.to_string());
            }
            scope = set.into_iter().collect::<Vec<_>>().join(" ");
        }
        if scope.is_empty() {
            return Err(AuthReject::InsufficientScope);
        }

        let subject = claims
            .preferred_username
            .or(claims.sub.clone())
            .unwrap_or_else(|| "unknown".into());
        let client_id = claims.client_id.unwrap_or_else(|| subject.clone());
        let audience = match claims.aud {
            Aud::One(s) => s,
            Aud::Many(v) => v
                .into_iter()
                .find(|a| self.audiences.iter().any(|e| e == a))
                .unwrap_or_else(|| self.resource.clone()),
        };

        Ok(AuthIdentity {
            subject: subject.clone(),
            client_id,
            scope,
            groups,
            issuer: claims.iss,
            audience,
            iat: claims.iat.unwrap_or_else(|| Utc::now().timestamp()),
            exp: claims.exp,
            jti: claims.jti.unwrap_or_else(|| Uuid::new_v4().to_string()),
            source: AuthSource::AuthentikJwt,
        })
    }

    fn extract_groups(&self, claims: &AuthentikClaims) -> Vec<String> {
        let value = claims
            .extra
            .get(&self.groups_claim)
            .or_else(|| claims.extra.get("groups"));
        match value {
            Some(serde_json::Value::String(s)) => split_groups(s),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect(),
            _ => claims
                .groups
                .as_ref()
                .map(|g| {
                    g.iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AuthentikClaims {
    iss: String,
    aud: Aud,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    iat: Option<i64>,
    exp: i64,
    #[serde(default)]
    jti: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::AuthSource;
    use axum::http::{HeaderMap, HeaderValue};

    fn cfg_forward() -> AuthentikConfig {
        let mut group_scopes = BTreeMap::new();
        group_scopes.insert("mcp-users".into(), "mcp:use".into());
        group_scopes.insert("architect".into(), "mcp:use upstream:architect_c4".into());
        AuthentikConfig {
            issuer: "https://auth.example.com/application/o/mcp-internal/".into(),
            jwks_url: "https://auth.example.com/application/o/mcp-internal/jwks/".into(),
            audiences: vec!["https://mcp.example.com/mcp".into()],
            resource: "https://mcp.example.com/mcp".into(),
            accept_bearer: false,
            forward_auth: true,
            group_scopes,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn forward_auth_requires_username_header() {
        let auth = AuthentikAuth::new(cfg_forward()).unwrap();
        let err = auth.authenticate(&HeaderMap::new()).await.unwrap_err();
        assert_eq!(
            err,
            AuthReject::MissingForwardAuthHeader("X-authentik-username")
        );
    }

    #[tokio::test]
    async fn forward_auth_maps_exact_groups_every_request() {
        let auth = AuthentikAuth::new(cfg_forward()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-authentik-username", HeaderValue::from_static("alice"));
        // architect-x must NOT get architect rights (delimiter + exact match).
        headers.insert(
            "x-authentik-groups",
            HeaderValue::from_static("architect-x|mcp-users"),
        );

        let id = auth.authenticate(&headers).await.unwrap();
        assert_eq!(id.subject, "alice");
        assert_eq!(id.scope, "mcp:use");
        assert_eq!(id.source, AuthSource::AuthentikForwardAuth);
        assert!(!id.scope.contains("architect_c4"));
    }

    #[tokio::test]
    async fn forward_auth_rejects_when_no_mapped_scopes() {
        let auth = AuthentikAuth::new(cfg_forward()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-authentik-username", HeaderValue::from_static("bob"));
        headers.insert(
            "x-authentik-groups",
            HeaderValue::from_static("unrelated-group"),
        );
        let err = auth.authenticate(&headers).await.unwrap_err();
        assert_eq!(err, AuthReject::InsufficientScope);
    }

    #[tokio::test]
    async fn bearer_disabled_and_forward_disabled_requires_bearer() {
        let auth = AuthentikAuth::new(AuthentikConfig {
            accept_bearer: false,
            forward_auth: false,
            ..cfg_forward()
        })
        .unwrap();
        let err = auth.authenticate(&HeaderMap::new()).await.unwrap_err();
        assert_eq!(err, AuthReject::MissingBearer);
    }

    /// Local JWKS HTTP mock. Aborts the accept loop on drop (no task leak in tests).
    struct JwksServer {
        url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for JwksServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn spawn_jwks_server(jwks_json: serde_json::Value) -> JwksServer {
        use axum::{routing::get, Json, Router};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/jwks",
            get(move || {
                let body = jwks_json.clone();
                async move { Json(body) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        JwksServer {
            url: format!("http://{addr}/jwks"),
            handle,
        }
    }

    fn sign_jwt(mgr: &crate::jwks::JwksManager, claims: serde_json::Value) -> String {
        use jsonwebtoken::{encode, Algorithm, Header};
        let cur = mgr.current.load();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(cur.kid.clone());
        encode(&header, &claims, &cur.encoding).unwrap()
    }

    #[tokio::test]
    async fn jwt_bearer_maps_groups_and_merges_scopes() {
        use crate::jwks::JwksManager;
        use axum::http::header;

        let mgr = JwksManager::new_with_fresh("ak-jwt").unwrap();
        let jwks_body = serde_json::json!({ "keys": mgr.jwks() });
        let server = spawn_jwks_server(jwks_body).await;

        let issuer = "https://auth.test/application/o/mcp/";
        let audience = "https://mcp.test/mcp";
        let mut group_scopes = BTreeMap::new();
        group_scopes.insert("mcp-users".into(), "mcp:use".into());
        group_scopes.insert("architect".into(), "upstream:architect_c4".into());

        let auth = AuthentikAuth::new(AuthentikConfig {
            issuer: issuer.into(),
            jwks_url: server.url.clone(),
            audiences: vec![audience.into()],
            resource: audience.into(),
            accept_bearer: true,
            forward_auth: false,
            groups_claim: "groups".into(),
            group_scopes,
            ..Default::default()
        })
        .unwrap();

        let now = Utc::now().timestamp();
        let token = sign_jwt(
            &mgr,
            serde_json::json!({
                "iss": issuer,
                "aud": [audience, "extra-aud"],
                "sub": "u1",
                "preferred_username": "alice",
                "client_id": "cursor",
                "scope": "mcp:read",
                "groups": ["mcp-users", "architect"],
                "iat": now,
                "exp": now + 3600,
                "jti": "jti-1",
            }),
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        let id = auth.authenticate(&headers).await.unwrap();
        assert_eq!(id.source, AuthSource::AuthentikJwt);
        assert_eq!(id.subject, "alice");
        assert_eq!(id.client_id, "cursor");
        assert!(id.scope.contains("mcp:read"));
        assert!(id.scope.contains("mcp:use"));
        assert!(id.scope.contains("upstream:architect_c4"));
        assert_eq!(id.audience, audience);
    }

    #[tokio::test]
    async fn jwt_groups_from_string_claim_and_insufficient_scope() {
        use crate::jwks::JwksManager;
        use axum::http::header;

        let mgr = JwksManager::new_with_fresh("ak-jwt2").unwrap();
        let server = spawn_jwks_server(serde_json::json!({ "keys": mgr.jwks() })).await;
        let issuer = "https://auth.test/application/o/mcp/";
        let audience = "https://mcp.test/mcp";
        let mut group_scopes = BTreeMap::new();
        group_scopes.insert("mcp-users".into(), "mcp:use".into());

        let auth = AuthentikAuth::new(AuthentikConfig {
            issuer: issuer.into(),
            jwks_url: server.url.clone(),
            audiences: vec![audience.into()],
            resource: audience.into(),
            accept_bearer: true,
            forward_auth: true,
            groups_claim: "ak_groups".into(),
            group_scopes,
            ..Default::default()
        })
        .unwrap();

        let now = Utc::now().timestamp();
        let good = sign_jwt(
            &mgr,
            serde_json::json!({
                "iss": issuer,
                "aud": audience,
                "sub": "u2",
                "ak_groups": "mcp-users|other",
                "iat": now,
                "exp": now + 3600,
            }),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {good}")).unwrap(),
        );
        let id = auth.authenticate(&headers).await.unwrap();
        assert_eq!(id.scope, "mcp:use");
        assert_eq!(id.subject, "u2");

        let empty = sign_jwt(
            &mgr,
            serde_json::json!({
                "iss": issuer,
                "aud": audience,
                "sub": "u3",
                "groups": ["unrelated"],
                "iat": now,
                "exp": now + 3600,
            }),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {empty}")).unwrap(),
        );
        assert_eq!(
            auth.authenticate(&headers).await.unwrap_err(),
            AuthReject::InsufficientScope
        );

        let bad = sign_jwt(
            &mgr,
            serde_json::json!({
                "iss": "https://evil.example/",
                "aud": audience,
                "sub": "u4",
                "groups": ["mcp-users"],
                "iat": now,
                "exp": now + 3600,
            }),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {bad}")).unwrap(),
        );
        assert_eq!(
            auth.authenticate(&headers).await.unwrap_err(),
            AuthReject::InvalidToken
        );
    }

    #[tokio::test]
    async fn invalid_header_names_rejected_at_construct() {
        let err = AuthentikAuth::new(AuthentikConfig {
            username_header: "bad header".into(),
            ..cfg_forward()
        })
        .err()
        .expect("invalid header name must fail");
        assert!(err.to_string().contains("username_header"));
    }
}
