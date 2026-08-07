//! Trust boundary for Authentik forward-auth headers.
//!
//! `X-authentik-*` must only be accepted from a proven hop: TCP peer in
//! `trusted_proxies` and/or a shared secret header injected by the gateway.
//! Arbitrary internet clients that can reach the pod must not mint sessions.

use std::net::IpAddr;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use ipnet::IpNet;

use crate::facade::AuthReject;

/// Parsed forward-auth hop trust policy.
#[derive(Debug, Clone)]
pub struct ForwardAuthTrust {
    /// Allowed TCP peer CIDRs (empty → peer check disabled).
    pub proxies: Vec<IpNet>,
    /// Shared secret the gateway must send (empty → secret check disabled).
    pub secret: String,
    /// Header carrying the shared secret.
    pub secret_header: HeaderName,
}

impl ForwardAuthTrust {
    /// Build from config strings. Errors on invalid CIDR / header name.
    pub fn new(
        trusted_proxies: &[String],
        secret: &str,
        secret_header: &str,
    ) -> anyhow::Result<Self> {
        let mut proxies = Vec::with_capacity(trusted_proxies.len());
        for raw in trusted_proxies {
            let net: IpNet = raw
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid trusted_proxies entry `{raw}`: {e}"))?;
            proxies.push(net);
        }
        let secret_header = HeaderName::from_bytes(secret_header.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid forward_auth_secret_header: {e}"))?;
        Ok(Self {
            proxies,
            secret: secret.to_string(),
            secret_header,
        })
    }

    /// Whether any trust mechanism is configured.
    pub fn is_configured(&self) -> bool {
        !self.proxies.is_empty() || !self.secret.is_empty()
    }

    /// Verify the hop before trusting identity headers.
    ///
    /// - Secret configured → header must match (constant-time).
    /// - Proxies configured → `peer` must be present and in a CIDR.
    /// - Both → both must pass.
    /// - Neither → [`AuthReject::UntrustedForwardAuth`] (fail closed).
    pub fn verify(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Result<(), AuthReject> {
        if !self.is_configured() {
            return Err(AuthReject::UntrustedForwardAuth);
        }

        if !self.secret.is_empty() {
            let presented = headers
                .get(&self.secret_header)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !ct_eq(presented.as_bytes(), self.secret.as_bytes()) {
                return Err(AuthReject::InvalidForwardAuthSecret);
            }
        }

        if !self.proxies.is_empty() {
            let Some(ip) = peer else {
                return Err(AuthReject::UntrustedForwardAuthPeer);
            };
            if !self.proxies.iter().any(|net| net.contains(&ip)) {
                return Err(AuthReject::UntrustedForwardAuthPeer);
            }
        }

        Ok(())
    }
}

/// Constant-time equality for hop secrets (length mismatch → false).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Helper for tests / admin: insert secret header.
pub fn insert_secret_header(headers: &mut HeaderMap, name: &HeaderName, secret: &str) {
    if let Ok(v) = HeaderValue::from_str(secret) {
        headers.insert(name.clone(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn trust_both() -> ForwardAuthTrust {
        ForwardAuthTrust::new(
            &["10.0.0.0/8".into(), "127.0.0.1/32".into()],
            "s3cr3t",
            "x-vmcp-forward-auth",
        )
        .unwrap()
    }

    #[test]
    fn rejects_when_nothing_configured() {
        let t = ForwardAuthTrust::new(&[], "", "x-vmcp-forward-auth").unwrap();
        assert!(!t.is_configured());
        assert_eq!(
            t.verify(&HeaderMap::new(), Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
            Err(AuthReject::UntrustedForwardAuth)
        );
    }

    #[test]
    fn secret_only_accepts_matching_header() {
        let t = ForwardAuthTrust::new(&[], "hop-secret", "x-vmcp-forward-auth").unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(
            t.verify(&headers, None),
            Err(AuthReject::InvalidForwardAuthSecret)
        );
        headers.insert("x-vmcp-forward-auth", HeaderValue::from_static("wrong"));
        assert_eq!(
            t.verify(&headers, None),
            Err(AuthReject::InvalidForwardAuthSecret)
        );
        headers.insert(
            "x-vmcp-forward-auth",
            HeaderValue::from_static("hop-secret"),
        );
        t.verify(&headers, None).unwrap();
    }

    #[test]
    fn proxies_only_require_matching_peer() {
        let t =
            ForwardAuthTrust::new(&["10.244.0.0/16".into()], "", "x-vmcp-forward-auth").unwrap();
        assert_eq!(
            t.verify(&HeaderMap::new(), None),
            Err(AuthReject::UntrustedForwardAuthPeer)
        );
        assert_eq!(
            t.verify(
                &HeaderMap::new(),
                Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
            ),
            Err(AuthReject::UntrustedForwardAuthPeer)
        );
        t.verify(
            &HeaderMap::new(),
            Some(IpAddr::V4(Ipv4Addr::new(10, 244, 1, 5))),
        )
        .unwrap();
    }

    #[test]
    fn both_require_secret_and_peer() {
        let t = trust_both();
        let mut headers = HeaderMap::new();
        headers.insert("x-vmcp-forward-auth", HeaderValue::from_static("s3cr3t"));
        assert_eq!(
            t.verify(&headers, Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))),
            Err(AuthReject::UntrustedForwardAuthPeer)
        );
        t.verify(&headers, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)))
            .unwrap();
    }
}
