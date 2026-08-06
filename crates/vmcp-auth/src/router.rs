//! OAuth 2.1 router: well-known metadata, DCR, authorize, consent, token, jwks.

use std::collections::BTreeMap;

use axum::{
    extract::{Form, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use chrono::Utc;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::password::verify_master;
use crate::state::AuthState;
use crate::tokens::issue_access_token;
use crate::types::*;

/// Protected-resource metadata only (Authentik / external AS mode).
///
/// MCP clients discover Authentik via `authorization_servers`; vmcp does not
/// mount local `/authorize`, `/token`, or DCR in this mode.
pub fn build_external_rs_router(
    resource: String,
    authorization_servers: Vec<String>,
    resource_audiences: Vec<String>,
) -> Router {
    let state = ExternalRsState {
        resource: resource.clone(),
        authorization_servers,
        resource_audiences,
    };
    let mut router = Router::new().route(
        "/.well-known/oauth-protected-resource",
        get(external_rs_metadata),
    );
    for aud in &state.resource_audiences {
        if let Ok(url) = url::Url::parse(aud) {
            let path = url.path();
            if path.len() > 1 {
                let route = format!("/.well-known/oauth-protected-resource{path}");
                router = router.route(&route, get(external_rs_metadata_scoped));
            }
        }
    }
    router.with_state(state)
}

#[derive(Clone)]
struct ExternalRsState {
    resource: String,
    authorization_servers: Vec<String>,
    resource_audiences: Vec<String>,
}

async fn external_rs_metadata(State(s): State<ExternalRsState>) -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource: s.resource.clone(),
        authorization_servers: s.authorization_servers.clone(),
        bearer_methods_supported: vec!["header"],
        resource_documentation: s.authorization_servers.first().cloned(),
    })
}

async fn external_rs_metadata_scoped(
    State(s): State<ExternalRsState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Result<Json<ProtectedResourceMetadata>, StatusCode> {
    const PREFIX: &str = "/.well-known/oauth-protected-resource";
    let suffix = uri.path().strip_prefix(PREFIX).unwrap_or("");
    if suffix.is_empty() || !suffix.starts_with('/') {
        return Err(StatusCode::NOT_FOUND);
    }
    let matched = s.resource_audiences.iter().find(|aud| {
        url::Url::parse(aud)
            .ok()
            .is_some_and(|u| u.path() == suffix)
    });
    match matched {
        Some(resource) => Ok(Json(ProtectedResourceMetadata {
            resource: resource.clone(),
            authorization_servers: s.authorization_servers.clone(),
            bearer_methods_supported: vec!["header"],
            resource_documentation: s.authorization_servers.first().cloned(),
        })),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Mount all OAuth-facing routes. None require authentication themselves —
/// authentication is for the MCP endpoint, layered separately by the bin
/// crate.
pub fn build_router(state: AuthState) -> Router {
    // RFC 9728 protected-resource metadata:
    //   - bare /.well-known/oauth-protected-resource (primary /mcp)
    //   - path-scoped /.well-known/oauth-protected-resource{mcp_path}
    //     for every accepted audience (/mcp, /mcp-proxy, …)
    // Middleware emits the bare URL in WWW-Authenticate; path-scoped routes
    // matter for clients that use Server URL = /mcp-proxy.
    let mut router = Router::new()
        .route("/.well-known/oauth-authorization-server", get(as_metadata))
        .route("/.well-known/oauth-protected-resource", get(rs_metadata))
        .route("/.well-known/jwks.json", get(jwks_endpoint))
        .route("/register", post(register_client))
        .route("/authorize", get(authorize))
        .route("/consent", get(consent_page).post(submit_consent))
        .route("/token", post(token_endpoint));

    for aud in &state.resource_audiences {
        if let Ok(url) = url::Url::parse(aud) {
            let path = url.path();
            if path.len() > 1 {
                let route = format!("/.well-known/oauth-protected-resource{path}");
                router = router.route(&route, get(rs_metadata_scoped));
            }
        }
    }

    router.with_state(state)
}

async fn as_metadata(State(s): State<AuthState>) -> Json<AuthorizationServerMetadata> {
    let base = s.issuer.trim_end_matches('/');
    Json(AuthorizationServerMetadata {
        issuer: s.issuer.clone(),
        authorization_endpoint: format!("{base}/authorize"),
        token_endpoint: format!("{base}/token"),
        registration_endpoint: format!("{base}/register"),
        jwks_uri: format!("{base}/.well-known/jwks.json"),
        response_types_supported: vec!["code"],
        grant_types_supported: vec!["authorization_code"],
        code_challenge_methods_supported: vec!["S256"],
        token_endpoint_auth_methods_supported: vec!["none"],
        scopes_supported: vec![
            s.default_scope.clone(),
            "mcp:admin".into(),
            "mcp:read".into(),
            "mcp:write".into(),
            "upstream:<name>".into(),
            "deny:<server>.<tool>".into(),
        ],
        resource_indicators_supported: true,
    })
}

fn protected_resource_metadata(s: &AuthState, resource: &str) -> ProtectedResourceMetadata {
    ProtectedResourceMetadata {
        resource: resource.to_string(),
        authorization_servers: vec![s.issuer.clone()],
        bearer_methods_supported: vec!["header"],
        resource_documentation: Some(format!("{}/", s.issuer.trim_end_matches('/'))),
    }
}

async fn rs_metadata(State(s): State<AuthState>) -> Json<ProtectedResourceMetadata> {
    Json(protected_resource_metadata(&s, &s.resource_audience))
}

/// Path-scoped PRM: `/.well-known/oauth-protected-resource/mcp-proxy` advertises
/// `resource=https://host/mcp-proxy` so Cursor Server URL `/mcp-proxy` works.
async fn rs_metadata_scoped(
    State(s): State<AuthState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Result<Json<ProtectedResourceMetadata>, StatusCode> {
    const PREFIX: &str = "/.well-known/oauth-protected-resource";
    let suffix = uri.path().strip_prefix(PREFIX).unwrap_or("");
    if suffix.is_empty() || !suffix.starts_with('/') {
        return Err(StatusCode::NOT_FOUND);
    }
    let matched = s.resource_audiences.iter().find(|aud| {
        url::Url::parse(aud)
            .ok()
            .is_some_and(|u| u.path() == suffix)
    });
    match matched {
        Some(resource) => Ok(Json(protected_resource_metadata(&s, resource))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn jwks_endpoint(State(s): State<AuthState>) -> Json<Jwks> {
    Json(Jwks {
        keys: s.jwks.jwks(),
    })
}

async fn register_client(
    State(s): State<AuthState>,
    Json(req): Json<ClientRegistrationRequest>,
) -> Result<Json<ClientRegistrationResponse>, AuthError> {
    if !s.dcr.enabled {
        return Err(AuthError::Forbidden(
            "dynamic client registration is disabled".into(),
        ));
    }
    if req.redirect_uris.is_empty() {
        return Err(AuthError::BadRequest("redirect_uris required".into()));
    }
    for uri in &req.redirect_uris {
        if !s.dcr.redirect_uri_allowed(uri) {
            return Err(AuthError::Forbidden(format!(
                "redirect_uri not allowed by policy: {uri}"
            )));
        }
    }

    // Normalize + validate scope before taking the registration lock.
    let scope = normalize_oauth_scope(req.scope.as_deref(), &s.default_scope)?;

    let _reg_lock = s.dcr_register_lock.lock().await;
    if s.dcr.max_clients > 0 && (s.clients.len() as u64) >= s.dcr.max_clients {
        return Err(AuthError::Forbidden(format!(
            "DCR client limit reached ({})",
            s.dcr.max_clients
        )));
    }
    let client_id = format!("vmcp-{}", Uuid::new_v4());
    let now = Utc::now();
    let grant_types = if req.grant_types.is_empty() {
        vec!["authorization_code".to_string()]
    } else {
        req.grant_types.clone()
    };
    let response_types = if req.response_types.is_empty() {
        vec!["code".to_string()]
    } else {
        req.response_types.clone()
    };
    // Operator label: slug(client_name) with -2/-3… until unique among DCR clients.
    let name = s.allocate_client_name(req.client_name.as_deref());
    let info = ClientInfo {
        client_id: client_id.clone(),
        redirect_uris: req.redirect_uris.clone(),
        client_name: req.client_name.clone(),
        name,
        grant_types: grant_types.clone(),
        response_types: response_types.clone(),
        scope: Some(scope.clone()),
        issued_at: now,
    };
    // Persist before the hot-cache insert so a failed write never leaves Cursor
    // holding a client_id the gateway will forget on the next request.
    if let Some(store) = s.client_store.as_ref() {
        store
            .upsert(&info)
            .map_err(|e| AuthError::Internal(format!("persist DCR client: {e}")))?;
    }
    s.clients.insert(client_id.clone(), info);

    tracing::info!(
        client_id = %client_id,
        client_name = ?req.client_name,
        redirect_uris = ?req.redirect_uris,
        "DCR client registered"
    );

    Ok(Json(ClientRegistrationResponse {
        client_id,
        redirect_uris: req.redirect_uris,
        client_name: req.client_name,
        token_endpoint_auth_method: req.token_endpoint_auth_method,
        grant_types,
        response_types,
        scope: Some(scope),
        client_id_issued_at: now.timestamp(),
    }))
}

/// Drop `mcp:admin` from DCR/OAuth scopes (G30). Admin is only for
/// pre-reg / static operator tokens.
fn sanitize_dcr_scope(scope: &str) -> String {
    scope
        .split_whitespace()
        .filter(|t| *t != "mcp:admin")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitize + validate OAuth scope for DCR/authorize. Empty after strip → default.
fn normalize_oauth_scope(
    requested: Option<&str>,
    default_scope: &str,
) -> Result<String, AuthError> {
    let raw = requested.unwrap_or(default_scope);
    let cleaned = sanitize_dcr_scope(raw);
    let scope = if cleaned.is_empty() {
        default_scope.to_string()
    } else {
        cleaned
    };
    crate::scopes::validate_scope_string(&scope).map_err(AuthError::BadRequest)?;
    Ok(scope)
}

#[derive(Debug, Deserialize)]
struct AuthorizeParams {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    code_challenge: String,
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn authorize(
    State(s): State<AuthState>,
    Query(p): Query<AuthorizeParams>,
) -> Result<Redirect, AuthError> {
    if p.response_type != "code" {
        return Err(AuthError::BadRequest(format!(
            "unsupported response_type: {}",
            p.response_type
        )));
    }
    if p.code_challenge_method != "S256" {
        return Err(AuthError::BadRequest("only S256 PKCE supported".into()));
    }
    let client = s
        .clients
        .get(&p.client_id)
        .ok_or_else(|| AuthError::BadRequest("unknown client_id".into()))?;
    if !client.redirect_uris.iter().any(|r| r == &p.redirect_uri) {
        return Err(AuthError::BadRequest("redirect_uri mismatch".into()));
    }
    drop(client);
    let scope = normalize_oauth_scope(p.scope.as_deref(), &s.default_scope)?;

    let consent = ConsentSession {
        id: format!("cs-{}", Uuid::new_v4()),
        client_id: p.client_id.clone(),
        redirect_uri: p.redirect_uri.clone(),
        state: p.state.clone(),
        scope,
        code_challenge: p.code_challenge.clone(),
        code_challenge_method: "S256".into(),
        resource: p.resource.clone(),
        created_at: Utc::now(),
    };
    let session_id = consent.id.clone();
    s.consents.insert(session_id.clone(), consent);

    let base = s.issuer.trim_end_matches('/');
    Ok(Redirect::to(&format!("{base}/consent?cs={session_id}")))
}

#[derive(Debug, Deserialize)]
struct ConsentQuery {
    cs: String,
}

async fn consent_page(
    State(s): State<AuthState>,
    Query(q): Query<ConsentQuery>,
) -> Result<Html<String>, AuthError> {
    let cs = s
        .consents
        .get(&q.cs)
        .ok_or_else(|| AuthError::BadRequest("expired consent session".into()))?;
    let client_label = match s.clients.get(&cs.client_id) {
        Some(c) => c.client_name.clone().unwrap_or_else(|| c.client_id.clone()),
        None => cs.client_id.clone(),
    };
    let html = render_consent_html(&q.cs, &client_label, &cs.scope);
    Ok(Html(html))
}

fn render_consent_html(session_id: &str, client_label: &str, scope: &str) -> String {
    // Static template — we keep things dependency-light. HTML-escaping is
    // limited because the inputs are server-controlled (client_label comes
    // from DCR, but we restrict its rendering surface).
    let escaped_label = html_escape(client_label);
    let escaped_scope = html_escape(scope);
    let escaped_session = html_escape(session_id);
    format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>vmcp — consent</title>
<style>
body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 32em; margin: 4em auto; padding: 0 1em; }}
h1 {{ color: #0f766e; }}
.client {{ background: #f3f4f6; padding: 1em; border-radius: 0.5em; margin: 1em 0; }}
form {{ display: grid; gap: 0.6em; margin-top: 1.5em; }}
input[type=password] {{ padding: 0.5em; font-size: 1em; }}
button {{ padding: 0.6em; background: #0f766e; color: white; border: 0; font-size: 1em; cursor: pointer; }}
button:hover {{ background: #115e59; }}
</style>
</head><body>
<h1>vmcp consent</h1>
<p>The application below is requesting access to your vmcp gateway.</p>
<div class="client">
  <strong>Client:</strong> {escaped_label}<br/>
  <strong>Scope:</strong> {escaped_scope}
</div>
<form method="POST" action="/consent">
  <input type="hidden" name="cs" value="{escaped_session}">
  <label>Master password:
    <input type="password" name="password" autofocus required>
  </label>
  <button type="submit">Grant access</button>
</form>
</body></html>"#
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[derive(Debug, Deserialize)]
struct ConsentForm {
    cs: String,
    password: String,
}

async fn submit_consent(
    State(s): State<AuthState>,
    Form(f): Form<ConsentForm>,
) -> Result<Redirect, AuthError> {
    // Look up but DON'T consume yet — a wrong password attempt must leave
    // the session intact so the operator can retype without restarting the
    // whole OAuth flow. Previously we removed first and verified second,
    // which made one typo silently torch the consent session: the next
    // POST then reported "expired consent session", indistinguishable from
    // an actually wrong password. argon2id's ~50 ms verify cost is the
    // brute-force speed bump.
    let session = s
        .consents
        .get(&f.cs)
        .map(|r| r.value().clone())
        .ok_or_else(|| AuthError::BadRequest("expired consent session".into()))?;

    let ok = verify_master(&f.password, &s.master_password_hash)
        .map_err(|e| AuthError::Internal(e.to_string()))?;
    if !ok {
        return Err(AuthError::Forbidden("invalid password".into()));
    }

    // Password verified — consume the session (one-time use).
    s.consents.remove(&f.cs);

    let code = format!("c-{}", Uuid::new_v4());
    let rec = AuthCodeRecord {
        code: code.clone(),
        client_id: session.client_id.clone(),
        redirect_uri: session.redirect_uri.clone(),
        code_challenge: session.code_challenge.clone(),
        code_challenge_method: session.code_challenge_method.clone(),
        scope: session.scope.clone(),
        resource: session.resource.clone(),
        issued_at: Utc::now(),
    };
    s.codes.insert(code.clone(), rec);

    // Append code + state to the redirect URI.
    let mut redirect = session.redirect_uri.clone();
    let sep = if redirect.contains('?') { '&' } else { '?' };
    redirect.push(sep);
    redirect.push_str("code=");
    redirect.push_str(&utf8_percent_encode(&code, NON_ALPHANUMERIC).to_string());
    if let Some(state) = &session.state {
        redirect.push_str("&state=");
        redirect.push_str(&utf8_percent_encode(state, NON_ALPHANUMERIC).to_string());
    }
    Ok(Redirect::to(&redirect))
}

async fn token_endpoint(
    State(s): State<AuthState>,
    Form(req): Form<BTreeMap<String, String>>,
) -> Result<Json<TokenResponse>, AuthError> {
    let grant_type = req
        .get("grant_type")
        .ok_or_else(|| AuthError::BadRequest("missing grant_type".into()))?;
    if grant_type != "authorization_code" {
        return Err(AuthError::BadRequest(format!(
            "unsupported grant_type: {grant_type}"
        )));
    }
    let code = req
        .get("code")
        .ok_or_else(|| AuthError::BadRequest("missing code".into()))?
        .clone();
    let code_verifier = req
        .get("code_verifier")
        .ok_or_else(|| AuthError::BadRequest("missing code_verifier".into()))?
        .clone();
    let client_id = req
        .get("client_id")
        .ok_or_else(|| AuthError::BadRequest("missing client_id".into()))?
        .clone();
    let redirect_uri = req
        .get("redirect_uri")
        .ok_or_else(|| AuthError::BadRequest("missing redirect_uri".into()))?
        .clone();
    let resource = req.get("resource").cloned();

    let rec = s
        .codes
        .remove(&code)
        .ok_or_else(|| AuthError::BadRequest("invalid code".into()))?
        .1;

    // TTL: 10 minutes.
    if (Utc::now() - rec.issued_at).num_seconds() > 600 {
        return Err(AuthError::BadRequest("expired code".into()));
    }
    if rec.client_id != client_id {
        return Err(AuthError::BadRequest("client_id mismatch".into()));
    }
    if rec.redirect_uri != redirect_uri {
        return Err(AuthError::BadRequest("redirect_uri mismatch".into()));
    }

    // PKCE: SHA256(code_verifier) base64url == code_challenge.
    let expected = pkce_s256(&code_verifier);
    if expected != rec.code_challenge {
        return Err(AuthError::BadRequest("PKCE verifier mismatch".into()));
    }

    // Resource indicator: accept any configured MCP mount (`/mcp`,
    // `/mcp-proxy`, …) or the bare public origin. Mint `aud` as the matched
    // mount URL (bare origin → primary `/mcp`) so clients that sent
    // `resource=…/mcp-proxy` get a matching JWT.
    let requested = resource.unwrap_or_else(|| s.resource_audience.clone());
    let Some(audience) = resolve_resource_audience(&requested, &s.resource_audiences) else {
        return Err(AuthError::BadRequest(
            "resource indicator does not match gateway".into(),
        ));
    };

    let (token, _claims) = issue_access_token(
        &s.jwks,
        &s.issuer,
        &audience,
        &rec.client_id,
        &rec.scope,
        s.token_ttl_secs,
    )
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    Ok(Json(TokenResponse {
        access_token: token,
        token_type: "Bearer",
        expires_in: s.token_ttl_secs,
        scope: rec.scope,
    }))
}

fn pkce_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Map a requested RFC 8707 `resource` to a JWT audience from `audiences`,
/// or `None` if it does not match any configured MCP mount (or their shared
/// bare origin). Bare origin (`https://host`) maps to the primary (first)
/// mount.
fn resolve_resource_audience(requested: &str, audiences: &[String]) -> Option<String> {
    let req = requested.trim_end_matches('/');
    for canonical in audiences {
        let canon = canonical.trim_end_matches('/');
        if req.eq_ignore_ascii_case(canon) {
            return Some(canonical.clone());
        }
    }
    if let Some(primary) = audiences.first() {
        let canon = primary.trim_end_matches('/');
        if let Some(idx) = canon.rfind('/') {
            let origin = &canon[..idx];
            if origin.contains("://") && !origin.ends_with(":/") && req.eq_ignore_ascii_case(origin)
            {
                return Some(primary.clone());
            }
        }
    }
    None
}

/// Public AuthError reused by handlers.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            AuthError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AuthError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            AuthError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        let body = serde_json::json!({"error": msg});
        let mut resp = (status, Json(body)).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_spec_vector() {
        // Test vector from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_s256(verifier), challenge);
    }

    #[test]
    fn resource_indicator_accepts_mcp_path_and_bare_origin() {
        let audiences = vec!["https://gateway.example.com/mcp".into()];
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/mcp", &audiences),
            Some("https://gateway.example.com/mcp".into())
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/mcp/", &audiences),
            Some("https://gateway.example.com/mcp".into())
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com", &audiences),
            Some("https://gateway.example.com/mcp".into())
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/", &audiences),
            Some("https://gateway.example.com/mcp".into())
        );
        assert_eq!(
            resolve_resource_audience("https://evil.example/mcp", &audiences),
            None
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/other", &audiences),
            None
        );
    }

    #[test]
    fn resource_indicator_accepts_mcp_proxy_mount() {
        let audiences = vec![
            "https://gateway.example.com/mcp".into(),
            "https://gateway.example.com/mcp-proxy".into(),
        ];
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/mcp-proxy", &audiences),
            Some("https://gateway.example.com/mcp-proxy".into())
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/mcp-proxy/", &audiences),
            Some("https://gateway.example.com/mcp-proxy".into())
        );
        assert_eq!(
            resolve_resource_audience("https://gateway.example.com/mcp", &audiences),
            Some("https://gateway.example.com/mcp".into())
        );
    }

    #[test]
    fn html_escape_handles_special_chars() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("\"x\""), "&quot;x&quot;");
    }

    fn test_auth_state() -> AuthState {
        use crate::jwks::JwksManager;
        let jwks = JwksManager::new_with_fresh("kid").unwrap();
        AuthState::new(
            jwks,
            "https://iss.example",
            "https://iss.example/mcp",
            3600,
            "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4",
        )
    }

    async fn post_register(
        state: AuthState,
        body: serde_json::Value,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::body::Body;
        use tower::ServiceExt;
        build_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn dcr_disabled_forbids_register() {
        use crate::state::DcrPolicy;
        let state = test_auth_state().with_dcr_policy(DcrPolicy {
            enabled: false,
            max_clients: 0,
            redirect_uri_allowlist: vec![],
        });
        let resp = post_register(
            state,
            serde_json::json!({
                "client_name": "x",
                "redirect_uris": ["http://127.0.0.1/cb"]
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn dcr_allowlist_rejects_foreign_redirect() {
        use crate::state::DcrPolicy;
        let state = test_auth_state().with_dcr_policy(DcrPolicy {
            enabled: true,
            max_clients: 10,
            redirect_uri_allowlist: vec!["http://127.0.0.1".into()],
        });
        let resp = post_register(
            state,
            serde_json::json!({
                "client_name": "x",
                "redirect_uris": ["https://evil.example/cb"]
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn dcr_max_clients_enforced() {
        use crate::state::DcrPolicy;
        let state = test_auth_state().with_dcr_policy(DcrPolicy {
            enabled: true,
            max_clients: 1,
            redirect_uri_allowlist: vec![],
        });
        let ok = post_register(
            state.clone(),
            serde_json::json!({
                "client_name": "one",
                "redirect_uris": ["http://127.0.0.1/cb"]
            }),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let denied = post_register(
            state,
            serde_json::json!({
                "client_name": "two",
                "redirect_uris": ["http://127.0.0.1/cb2"]
            }),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn dcr_admin_only_scope_falls_back_to_default() {
        let state = test_auth_state();
        let resp = post_register(
            state,
            serde_json::json!({
                "client_name": "adminish",
                "redirect_uris": ["http://127.0.0.1/cb"],
                "scope": "mcp:admin"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["scope"], "mcp:use");
    }

    #[tokio::test]
    async fn dcr_rejects_malformed_scope_tokens() {
        let state = test_auth_state();
        let resp = post_register(
            state,
            serde_json::json!({
                "client_name": "bad",
                "redirect_uris": ["http://127.0.0.1/cb"],
                "scope": "mcp:use deny:broken"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    async fn oneshot(
        state: AuthState,
        req: axum::http::Request<axum::body::Body>,
    ) -> axum::http::Response<axum::body::Body> {
        use tower::ServiceExt;
        build_router(state).oneshot(req).await.unwrap()
    }

    fn pkce_pair() -> (String, String) {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string();
        let challenge = pkce_s256(&verifier);
        (verifier, challenge)
    }

    #[tokio::test]
    async fn oauth_metadata_jwks_and_full_code_flow() {
        use axum::body::Body;

        let master = "oauth-master-secret";
        let hash = crate::password::hash_password(master).unwrap();
        let jwks = crate::jwks::JwksManager::new_with_fresh("oauth-flow").unwrap();
        let state = AuthState::new(
            jwks,
            "https://iss.example",
            "https://iss.example/mcp",
            3600,
            hash,
        )
        .with_extra_resource_audiences(vec!["https://iss.example/mcp-proxy".into()]);

        // Well-known + JWKS.
        for uri in [
            "/.well-known/oauth-authorization-server",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource/mcp-proxy",
            "/.well-known/jwks.json",
        ] {
            let resp = oneshot(
                state.clone(),
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
        }
        let miss = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri("/.well-known/oauth-protected-resource/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);

        // DCR register.
        let reg = post_register(
            state.clone(),
            serde_json::json!({
                "client_name": "Flow Client!!",
                "redirect_uris": ["http://127.0.0.1/cb?x=1"],
                "grant_types": ["authorization_code"],
                "response_types": ["code"]
            }),
        )
        .await;
        assert_eq!(reg.status(), StatusCode::OK);
        let reg_body = axum::body::to_bytes(reg.into_body(), 1 << 16)
            .await
            .unwrap();
        let reg_v: serde_json::Value = serde_json::from_slice(&reg_body).unwrap();
        let client_id = reg_v["client_id"].as_str().unwrap().to_string();

        let (verifier, challenge) = pkce_pair();
        let auth_uri = format!(
            "/authorize?client_id={client_id}&redirect_uri={}&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state=xyz&scope=mcp:use&resource=https://iss.example/mcp",
            urlencoding_path("http://127.0.0.1/cb?x=1")
        );
        let auth_resp = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri(&auth_uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(auth_resp.status(), StatusCode::SEE_OTHER);
        let loc = auth_resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(loc.contains("/consent?cs="));
        let cs = loc.split("cs=").nth(1).unwrap().to_string();

        // Consent page HTML.
        let page = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri(format!("/consent?cs={cs}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK);
        let html = axum::body::to_bytes(page.into_body(), 1 << 16).await.unwrap();
        assert!(String::from_utf8_lossy(&html).contains("vmcp consent"));

        // Wrong password leaves session intact.
        let bad = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/consent")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("cs={cs}&password=wrong")))
                .unwrap(),
        )
        .await;
        assert_eq!(bad.status(), StatusCode::FORBIDDEN);

        // Grant.
        let grant = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/consent")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("cs={cs}&password={master}")))
                .unwrap(),
        )
        .await;
        assert_eq!(grant.status(), StatusCode::SEE_OTHER);
        let cb = grant
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cb.contains("code="));
        assert!(cb.contains("state=xyz"));
        let code = cb
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let token_body = format!(
            "grant_type=authorization_code&code={code}&code_verifier={verifier}&client_id={client_id}&redirect_uri={}&resource=https://iss.example/mcp",
            urlencoding_path("http://127.0.0.1/cb?x=1")
        );
        let token_resp = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(token_body))
                .unwrap(),
        )
        .await;
        assert_eq!(token_resp.status(), StatusCode::OK);
        let tb = axum::body::to_bytes(token_resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let tv: serde_json::Value = serde_json::from_slice(&tb).unwrap();
        assert_eq!(tv["token_type"], "Bearer");
        assert!(tv["access_token"].as_str().unwrap().len() > 20);

        // Authorize rejects bad response_type / unknown client.
        let bad_rt = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri(format!(
                    "/authorize?client_id={client_id}&redirect_uri={}&response_type=token&code_challenge={challenge}&code_challenge_method=S256",
                    urlencoding_path("http://127.0.0.1/cb?x=1")
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(bad_rt.status(), StatusCode::BAD_REQUEST);

        let unk = oneshot(
            state,
            axum::http::Request::builder()
                .uri(format!(
                    "/authorize?client_id=nope&redirect_uri={}&response_type=code&code_challenge={challenge}&code_challenge_method=S256",
                    urlencoding_path("http://127.0.0.1/cb?x=1")
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(unk.status(), StatusCode::BAD_REQUEST);
    }

    fn urlencoding_path(s: &str) -> String {
        utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
    }

    #[tokio::test]
    async fn token_endpoint_rejects_bad_grant_and_pkce() {
        use axum::body::Body;

        let master = "tok-master";
        let hash = crate::password::hash_password(master).unwrap();
        let jwks = crate::jwks::JwksManager::new_with_fresh("tok").unwrap();
        let state = AuthState::new(jwks, "https://iss.example", "https://iss.example/mcp", 3600, hash);

        let reg = post_register(
            state.clone(),
            serde_json::json!({
                "client_name": "t",
                "redirect_uris": ["http://127.0.0.1/cb"]
            }),
        )
        .await;
        let reg_body = axum::body::to_bytes(reg.into_body(), 1 << 16)
            .await
            .unwrap();
        let client_id = serde_json::from_slice::<serde_json::Value>(&reg_body).unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (verifier, challenge) = pkce_pair();

        let auth = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri(format!(
                    "/authorize?client_id={client_id}&redirect_uri=http%3A%2F%2F127.0.0.1%2Fcb&response_type=code&code_challenge={challenge}&code_challenge_method=S256"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let cs = auth
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .split("cs=")
            .nth(1)
            .unwrap()
            .to_string();
        let grant = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/consent")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("cs={cs}&password={master}")))
                .unwrap(),
        )
        .await;
        let code = grant
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let bad_grant = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("grant_type=client_credentials"))
                .unwrap(),
        )
        .await;
        assert_eq!(bad_grant.status(), StatusCode::BAD_REQUEST);

        let bad_pkce = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code}&code_verifier=wrong-verifier-value-xx&client_id={client_id}&redirect_uri=http://127.0.0.1/cb"
                )))
                .unwrap(),
        )
        .await;
        assert_eq!(bad_pkce.status(), StatusCode::BAD_REQUEST);

        // Re-mint a fresh code for success with bare-origin resource.
        let auth2 = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .uri(format!(
                    "/authorize?client_id={client_id}&redirect_uri=http%3A%2F%2F127.0.0.1%2Fcb&response_type=code&code_challenge={challenge}&code_challenge_method=S256"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let cs2 = auth2
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .split("cs=")
            .nth(1)
            .unwrap()
            .to_string();
        let grant2 = oneshot(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/consent")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("cs={cs2}&password={master}")))
                .unwrap(),
        )
        .await;
        let code2 = grant2
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        let ok = oneshot(
            state,
            axum::http::Request::builder()
                .method("POST")
                .uri("/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code2}&code_verifier={verifier}&client_id={client_id}&redirect_uri=http://127.0.0.1/cb&resource=https://iss.example"
                )))
                .unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn external_rs_router_serves_prm_and_scoped_paths() {
        use axum::body::Body;
        use tower::ServiceExt;

        let app = build_external_rs_router(
            "https://mcp.example/mcp".into(),
            vec!["https://auth.example/application/o/mcp/".into()],
            vec![
                "https://mcp.example/mcp".into(),
                "https://mcp.example/mcp-proxy".into(),
            ],
        );

        let bare = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bare.status(), StatusCode::OK);
        let body = axum::body::to_bytes(bare.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["resource"], "https://mcp.example/mcp");
        assert_eq!(
            v["authorization_servers"][0],
            "https://auth.example/application/o/mcp/"
        );

        for path in ["/mcp", "/mcp-proxy"] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/.well-known/oauth-protected-resource{path}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "path {path}");
            let b = axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap();
            let j: serde_json::Value = serde_json::from_slice(&b).unwrap();
            assert_eq!(j["resource"], format!("https://mcp.example{path}"));
        }

        let miss = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/.well-known/oauth-protected-resource/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dcr_empty_redirect_uris_rejected_and_persist_path() {
        use crate::client_store::ClientStore;
        use std::sync::Arc;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db = dir.path().join("clients.db");
        let store = Arc::new(ClientStore::open(&db).unwrap());
        let state = test_auth_state().with_client_store(store).unwrap();

        let empty = post_register(
            state.clone(),
            serde_json::json!({
                "client_name": "x",
                "redirect_uris": []
            }),
        )
        .await;
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);

        let ok = post_register(
            state,
            serde_json::json!({
                "client_name": "persisted",
                "redirect_uris": ["http://127.0.0.1/cb"]
            }),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }
}
