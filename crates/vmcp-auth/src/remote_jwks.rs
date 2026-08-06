//! Remote JWKS fetch + `kid` cache over rustls-backed `reqwest`.
//!
//! Replaces `async-oidc-jwt-validator`, which pulled OpenSSL via
//! `reqwest` default `native-tls`. Signature verify stays in `jsonwebtoken`
//! (`ring`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, DecodingKey, Validation};
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;

/// HTTPS JWKS client with an in-memory `kid → Jwk` cache.
///
/// Cache miss for a `kid` triggers a refresh; unknown `kid` after refresh
/// rejects the token.
#[derive(Clone)]
pub struct RemoteJwks {
    url: String,
    http: reqwest::Client,
    cache: Arc<RwLock<HashMap<String, Jwk>>>,
}

impl RemoteJwks {
    pub fn new(jwks_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("vmcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build rustls reqwest client for JWKS")?;
        Ok(Self {
            url: jwks_url.into(),
            http,
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    async fn fetch(&self) -> Result<JwkSet> {
        let resp = self
            .http
            .get(&self.url)
            .send()
            .await
            .with_context(|| format!("GET {}", self.url))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("JWKS GET {} → HTTP {status}", self.url));
        }
        resp.json::<JwkSet>()
            .await
            .with_context(|| format!("decode JWKS JSON from {}", self.url))
    }

    async fn refresh(&self) -> Result<()> {
        let set = self.fetch().await?;
        let mut map = HashMap::with_capacity(set.keys.len());
        for jwk in set.keys {
            if let Some(kid) = jwk.common.key_id.clone() {
                map.insert(kid, jwk);
            }
        }
        let n = map.len();
        *self.cache.write().await = map;
        tracing::debug!(url = %self.url, keys = n, "refreshed remote JWKS cache");
        Ok(())
    }

    async fn key_for(&self, kid: &str) -> Result<Jwk> {
        {
            let cache = self.cache.read().await;
            if let Some(jwk) = cache.get(kid) {
                return Ok(jwk.clone());
            }
        }
        self.refresh().await?;
        self.cache
            .read()
            .await
            .get(kid)
            .cloned()
            .ok_or_else(|| anyhow!("JWKS has no key with kid={kid}"))
    }

    /// Decode `token` claims after JWKS lookup by `kid`.
    pub async fn decode_claims<T: DeserializeOwned>(
        &self,
        token: &str,
        validation: &Validation,
    ) -> Result<T> {
        let header = decode_header(token).context("JWT header")?;
        let kid = header
            .kid
            .ok_or_else(|| anyhow!("JWT missing kid header"))?;
        let jwk = self.key_for(&kid).await?;
        let key = DecodingKey::from_jwk(&jwk).context("DecodingKey::from_jwk")?;
        let data = decode::<T>(token, &key, validation).context("JWT signature/claims")?;
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwks::JwksManager;
    use axum::{routing::get, Json, Router};
    use chrono::Utc;
    use jsonwebtoken::{encode, Algorithm, Header};
    use tokio::net::TcpListener;

    struct JwksServer {
        url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for JwksServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn spawn_jwks(body: serde_json::Value) -> JwksServer {
        let app = Router::new().route(
            "/jwks",
            get(move || {
                let body = body.clone();
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

    #[tokio::test]
    async fn fetches_caches_and_verifies_rs256() {
        let mgr = JwksManager::new_with_fresh("remote").unwrap();
        let server = spawn_jwks(serde_json::json!({ "keys": mgr.jwks() })).await;
        let remote = RemoteJwks::new(server.url.clone()).unwrap();

        let now = Utc::now().timestamp();
        let cur = mgr.current.load();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(cur.kid.clone());
        let token = encode(
            &header,
            &serde_json::json!({
                "iss": "https://iss.test/",
                "aud": "https://mcp.test/mcp",
                "sub": "alice",
                "iat": now,
                "exp": now + 3600,
            }),
            &cur.encoding,
        )
        .unwrap();

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&["https://iss.test/"]);
        validation.set_audience(&["https://mcp.test/mcp"]);

        #[derive(Debug, serde::Deserialize)]
        struct C {
            sub: String,
        }
        let claims: C = remote.decode_claims(&token, &validation).await.unwrap();
        assert_eq!(claims.sub, "alice");

        // Second call hits cache (no error even if we don't assert network).
        let _: C = remote.decode_claims(&token, &validation).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_kid_after_refresh_is_error() {
        let mgr = JwksManager::new_with_fresh("remote2").unwrap();
        let server = spawn_jwks(serde_json::json!({ "keys": mgr.jwks() })).await;
        let remote = RemoteJwks::new(server.url.clone()).unwrap();

        let now = Utc::now().timestamp();
        let cur = mgr.current.load();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("not-in-jwks".into());
        // Sign with real key but advertise a foreign kid → cache miss + refresh miss.
        let token = encode(
            &header,
            &serde_json::json!({
                "iss": "https://iss.test/",
                "aud": "https://mcp.test/mcp",
                "sub": "x",
                "exp": now + 3600,
            }),
            &cur.encoding,
        )
        .unwrap();

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&["https://iss.test/"]);
        validation.set_audience(&["https://mcp.test/mcp"]);
        validation.validate_exp = true;

        let err = remote
            .decode_claims::<serde_json::Value>(&token, &validation)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("kid"), "unexpected error: {err:#}");
    }
}
