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
        scopes_supported: vec![s.default_scope.clone()],
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
    if req.redirect_uris.is_empty() {
        return Err(AuthError::BadRequest("redirect_uris required".into()));
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
        scope: req.scope.clone(),
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

    Ok(Json(ClientRegistrationResponse {
        client_id,
        redirect_uris: req.redirect_uris,
        client_name: req.client_name,
        token_endpoint_auth_method: req.token_endpoint_auth_method,
        grant_types,
        response_types,
        scope: req.scope.or_else(|| Some(s.default_scope.clone())),
        client_id_issued_at: now.timestamp(),
    }))
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
    let scope = p.scope.clone().unwrap_or_else(|| s.default_scope.clone());
    drop(client);

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
}
