//! Wire types for OAuth endpoints. Field naming matches RFC 6749 / 7591 / 8414
//! exactly — these are the JSON payloads we emit on the wire.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// RFC 7591 §3.1 Dynamic Client Registration request.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRegistrationRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    /// Per RFC 8252 / OAuth 2.1, public clients use `none`.
    #[serde(default = "default_auth_method")]
    pub token_endpoint_auth_method: String,
    #[serde(default)]
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub response_types: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

fn default_auth_method() -> String {
    "none".into()
}

/// RFC 7591 §3.2.1 successful response.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: Option<String>,
    pub client_id_issued_at: i64,
}

/// In-memory record for a registered client.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: Option<String>,
    pub issued_at: DateTime<Utc>,
}

/// RFC 6749 §4.1.3 token request (authorization_code).
#[derive(Debug, Clone, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    /// RFC 7636 — PKCE.
    #[serde(default)]
    pub code_verifier: Option<String>,
    /// RFC 8707 — Resource Indicator.
    #[serde(default)]
    pub resource: Option<String>,
}

/// RFC 6749 §5.1 token response.
#[derive(Debug, Clone, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str, // always "Bearer"
    pub expires_in: u64,
    pub scope: String,
}

/// In-memory authorization code record (one-shot, short-lived).
#[derive(Debug, Clone)]
pub struct AuthCodeRecord {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String, // we only support "S256"
    pub scope: String,
    pub resource: Option<String>,
    pub issued_at: DateTime<Utc>,
}

/// Pending consent session bound to a browser turn.
#[derive(Debug, Clone)]
pub struct ConsentSession {
    pub id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub scope: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub resource: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// RFC 8414 Authorization Server metadata document.
#[derive(Debug, Clone, Serialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub jwks_uri: String,
    pub response_types_supported: Vec<&'static str>,
    pub grant_types_supported: Vec<&'static str>,
    pub code_challenge_methods_supported: Vec<&'static str>,
    pub token_endpoint_auth_methods_supported: Vec<&'static str>,
    pub scopes_supported: Vec<String>,
    pub resource_indicators_supported: bool,
}

/// RFC 9728 Protected Resource metadata document.
#[derive(Debug, Clone, Serialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<&'static str>,
    pub resource_documentation: Option<String>,
}

/// JWT claims for vmcp access tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,        // = client_id
    pub client_id: String,
    pub scope: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

/// JWKS document.
#[derive(Debug, Clone, Serialize)]
pub struct Jwks {
    pub keys: Vec<JwkPublicKey>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JwkPublicKey {
    pub kty: &'static str, // "RSA"
    pub kid: String,
    #[serde(rename = "use")]
    pub use_: &'static str, // "sig"
    pub alg: &'static str, // "RS256"
    pub n: String,
    pub e: String,
}

/// Pull anything extra from query/form maps without panicking.
pub fn get_param<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key).map(|s| s.as_str())
}
