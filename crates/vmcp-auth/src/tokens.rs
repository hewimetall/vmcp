//! JWT issuance and verification.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, Header, Validation};
use uuid::Uuid;

use crate::jwks::JwksManager;
use crate::types::AccessTokenClaims;

/// Issue a fresh access token. Audience MUST be a resource indicator (RFC
/// 8707) — typically the canonical MCP endpoint URL.
pub fn issue_access_token(
    mgr: &JwksManager,
    issuer: &str,
    audience: &str,
    client_id: &str,
    scope: &str,
    ttl_secs: u64,
) -> Result<(String, AccessTokenClaims)> {
    let cur = mgr.current.load_full();
    let now = Utc::now().timestamp();
    let claims = AccessTokenClaims {
        iss: issuer.into(),
        aud: audience.into(),
        sub: client_id.into(),
        client_id: client_id.into(),
        scope: scope.into(),
        iat: now,
        exp: now + ttl_secs as i64,
        jti: Uuid::new_v4().to_string(),
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(cur.kid.clone());
    let token = encode(&header, &claims, &cur.encoding).context("jwt encode")?;
    Ok((token, claims))
}

/// Verify a bearer token. Returns the claims on success. Audience must match
/// one of `expected_audiences` (e.g. both `/mcp` and `/mcp-proxy`).
pub fn verify_access_token(
    mgr: &JwksManager,
    bearer: &str,
    expected_issuer: &str,
    expected_audiences: &[&str],
) -> Result<AccessTokenClaims> {
    let header = jsonwebtoken::decode_header(bearer).context("decode header")?;
    let kid = header.kid.ok_or_else(|| anyhow!("token missing `kid`"))?;
    let key: DecodingKey = mgr
        .decoding_for(&kid)
        .ok_or_else(|| anyhow!("unknown signing key id"))?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(expected_audiences);
    validation.set_issuer(&[expected_issuer]);
    validation.validate_aud = true;
    validation.validate_exp = true;

    let data = decode::<AccessTokenClaims>(bearer, &key, &validation).context("jwt verify")?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_and_verify_round_trip() {
        let mgr = JwksManager::new_with_fresh("kid-1").unwrap();
        let (tok, _) = issue_access_token(
            &mgr,
            "https://iss",
            "https://iss/mcp",
            "client-x",
            "mcp:use",
            60,
        )
        .unwrap();
        let claims = verify_access_token(&mgr, &tok, "https://iss", &["https://iss/mcp"]).unwrap();
        assert_eq!(claims.client_id, "client-x");
        assert_eq!(claims.aud, "https://iss/mcp");
    }

    #[test]
    fn audience_mismatch_rejected() {
        let mgr = JwksManager::new_with_fresh("kid-1").unwrap();
        let (tok, _) =
            issue_access_token(&mgr, "https://iss", "https://iss/mcp", "x", "s", 60).unwrap();
        let res = verify_access_token(&mgr, &tok, "https://iss", &["https://other/mcp"]);
        assert!(res.is_err());
    }

    #[test]
    fn either_mcp_or_proxy_audience_accepted() {
        let mgr = JwksManager::new_with_fresh("kid-1").unwrap();
        let (tok, _) =
            issue_access_token(&mgr, "https://iss", "https://iss/mcp-proxy", "x", "s", 60).unwrap();
        let claims = verify_access_token(
            &mgr,
            &tok,
            "https://iss",
            &["https://iss/mcp", "https://iss/mcp-proxy"],
        )
        .unwrap();
        assert_eq!(claims.aud, "https://iss/mcp-proxy");
    }

    #[test]
    fn previous_key_still_verifies_after_rotation() {
        let mgr = JwksManager::new_with_fresh("kid-1").unwrap();
        let (tok, _) =
            issue_access_token(&mgr, "https://iss", "https://iss/mcp", "x", "s", 60).unwrap();
        mgr.rotate("kid-2").unwrap();
        // Same token must still verify against the previous-key window.
        let claims = verify_access_token(&mgr, &tok, "https://iss", &["https://iss/mcp"]).unwrap();
        assert_eq!(claims.client_id, "x");
    }
}
