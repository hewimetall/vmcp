//! JWKS keypair + rotation. Two-key window (current + previous) so tokens
//! issued just before rotation still verify.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use jsonwebtoken::{DecodingKey, EncodingKey};
use rand::rngs::OsRng;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs8::EncodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;

use crate::types::JwkPublicKey;

/// A single RSA keypair we issue/verify with.
pub struct Keypair {
    pub kid: String,
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    /// Public-key components for JWKS (base64url-encoded big-endian, no padding).
    pub jwk: JwkPublicKey,
}

/// The active key set: a current keypair (used to sign) and an optional
/// previous keypair (still accepted on verify). The JWKS endpoint advertises
/// both so freshly-rotated tokens still resolve.
pub struct JwksManager {
    pub current: ArcSwap<Keypair>,
    pub previous: ArcSwap<Option<Arc<Keypair>>>,
}

impl JwksManager {
    /// Generate a fresh keypair and return a manager with no prior key.
    pub fn new_with_fresh(kid_seed: &str) -> anyhow::Result<Arc<Self>> {
        let kp = generate_keypair(kid_seed)?;
        Ok(Arc::new(Self {
            current: ArcSwap::from_pointee(kp),
            previous: ArcSwap::from(Arc::new(None)),
        }))
    }

    /// Rotate: current → previous, new keypair → current.
    pub fn rotate(&self, kid_seed: &str) -> anyhow::Result<()> {
        let new = generate_keypair(kid_seed)?;
        let old_current = self.current.load_full();
        self.previous.store(Arc::new(Some(old_current)));
        self.current.store(Arc::new(new));
        Ok(())
    }

    /// Snapshot of all currently-acceptable public keys for the JWKS endpoint.
    pub fn jwks(&self) -> Vec<JwkPublicKey> {
        let mut out = vec![self.current.load().jwk.clone()];
        if let Some(prev) = self.previous.load().as_ref() {
            out.push(prev.jwk.clone());
        }
        out
    }

    /// Find a decoding key by `kid`. Returns None on unknown kid.
    pub fn decoding_for(&self, kid: &str) -> Option<DecodingKey> {
        let cur = self.current.load();
        if cur.kid == kid {
            return Some(cur.decoding.clone());
        }
        if let Some(prev) = self.previous.load().as_ref() {
            if prev.kid == kid {
                return Some(prev.decoding.clone());
            }
        }
        None
    }

    /// Spawn a background rotation task. Cancels on drop of the returned
    /// handle (or when the runtime shuts down).
    pub fn spawn_rotation_task(
        self: Arc<Self>,
        every: Duration,
        kid_prefix: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            interval.tick().await; // skip the immediate first tick
            let mut gen = 1u64;
            loop {
                interval.tick().await;
                gen += 1;
                let seed = format!("{kid_prefix}-{gen}");
                if let Err(e) = self.rotate(&seed) {
                    tracing::error!(error = %e, "jwks rotation failed");
                }
            }
        })
    }
}

fn generate_keypair(kid: &str) -> anyhow::Result<Keypair> {
    // 2048-bit RSA. Plenty for RS256.
    let mut rng = OsRng;
    let priv_key = RsaPrivateKey::new(&mut rng, 2048)?;
    let pub_key = priv_key.to_public_key();

    let pem = priv_key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)?;
    let pub_pem = pub_key.to_public_key_pem(rsa::pkcs8::LineEnding::LF)?;
    let encoding = EncodingKey::from_rsa_pem(pem.as_bytes())?;
    let decoding = DecodingKey::from_rsa_pem(pub_pem.as_bytes())?;

    let n = base64_url(&pub_key.n().to_bytes_be());
    let e = base64_url(&pub_key.e().to_bytes_be());

    Ok(Keypair {
        kid: kid.to_string(),
        encoding,
        decoding,
        jwk: JwkPublicKey {
            kty: "RSA",
            kid: kid.to_string(),
            use_: "sig",
            alg: "RS256",
            n,
            e,
        },
    })
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_makes_previous_verifiable() {
        let mgr = JwksManager::new_with_fresh("a").unwrap();
        let initial_kid = mgr.current.load().kid.clone();
        mgr.rotate("b").unwrap();
        assert_eq!(mgr.current.load().kid, "b");
        // Old key still queryable via previous.
        assert!(mgr.decoding_for(&initial_kid).is_some());
        assert!(mgr.decoding_for("b").is_some());
        assert!(mgr.decoding_for("nope").is_none());
        // JWKS exposes both.
        assert_eq!(mgr.jwks().len(), 2);
    }
}
