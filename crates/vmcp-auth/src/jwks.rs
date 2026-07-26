//! JWKS keypair + rotation. Two-key window (current + previous) so tokens
//! issued just before rotation still verify.
//!
//! Optional on-disk persistence (`auth.jwks_private_key_pem_path`):
//! - current PKCS#1 PEM at the configured path
//! - atomic JSON bundle at `<path>.bundle.json` with current **and previous**
//!   keys so the rotation window survives pod restarts (G14 / G31).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use jsonwebtoken::{DecodingKey, EncodingKey};
use rand::rngs::OsRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
use rsa::pkcs8::EncodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};

use crate::types::JwkPublicKey;

/// A single RSA keypair we issue/verify with.
pub struct Keypair {
    pub kid: String,
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    /// Public-key components for JWKS (base64url-encoded big-endian, no padding).
    pub jwk: JwkPublicKey,
    /// PKCS#1 PEM of the private key (for optional disk persistence).
    pub private_pem: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JwksBundle {
    current_kid: String,
    current_pem: String,
    previous_kid: Option<String>,
    previous_pem: Option<String>,
}

/// The active key set: a current keypair (used to sign) and an optional
/// previous keypair (still accepted on verify). The JWKS endpoint advertises
/// both so freshly-rotated tokens still resolve.
pub struct JwksManager {
    pub current: ArcSwap<Keypair>,
    pub previous: ArcSwap<Option<Arc<Keypair>>>,
    /// When set, `rotate` and first boot write the current private key here.
    persist_path: Option<PathBuf>,
}

impl JwksManager {
    /// Generate a fresh keypair and return a manager with no prior key.
    /// Ephemeral — equivalent to [`Self::open`] with `persist = None`.
    pub fn new_with_fresh(kid_seed: &str) -> Result<Arc<Self>> {
        Self::open(kid_seed, None)
    }

    /// Open a manager: load bundle/PEM from `persist` if present, else generate
    /// (and write when `persist` is `Some`).
    pub fn open(kid_seed: &str, persist: Option<&Path>) -> Result<Arc<Self>> {
        let persist_path = persist.map(|p| p.to_path_buf());
        let (current, previous) = match &persist_path {
            Some(path) => load_persisted(path, kid_seed)?,
            None => (generate_keypair(kid_seed)?, None),
        };
        Ok(Arc::new(Self {
            current: ArcSwap::from_pointee(current),
            previous: ArcSwap::from(Arc::new(previous.map(Arc::new))),
            persist_path,
        }))
    }

    /// Rotate: current → previous, new keypair → current. Persists atomically
    /// when configured (bundle JSON + current PEM).
    pub fn rotate(&self, kid_seed: &str) -> Result<()> {
        let new = generate_keypair(kid_seed)?;
        let old_current = self.current.load_full();
        if let Some(path) = &self.persist_path {
            persist_atomic(path, &new, Some(old_current.as_ref()))?;
            tracing::info!(
                path = %path.display(),
                kid = %new.kid,
                prev_kid = %old_current.kid,
                "persisted rotated JWKS key bundle"
            );
        }
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

fn bundle_path(pem_path: &Path) -> PathBuf {
    let mut p = pem_path.as_os_str().to_os_string();
    p.push(".bundle.json");
    PathBuf::from(p)
}

fn load_persisted(path: &Path, kid_seed: &str) -> Result<(Keypair, Option<Keypair>)> {
    let bundle_file = bundle_path(path);
    if bundle_file.exists() {
        tracing::info!(path = %bundle_file.display(), "loading JWKS key bundle from disk");
        let text = std::fs::read_to_string(&bundle_file)
            .with_context(|| format!("read JWKS bundle {}", bundle_file.display()))?;
        let bundle: JwksBundle = serde_json::from_str(&text)
            .with_context(|| format!("parse JWKS bundle {}", bundle_file.display()))?;
        let current = keypair_from_pem_str(&bundle.current_pem, &bundle.current_kid)?;
        let previous = match (bundle.previous_pem, bundle.previous_kid) {
            (Some(pem), Some(kid)) => Some(keypair_from_pem_str(&pem, &kid)?),
            _ => None,
        };
        return Ok((current, previous));
    }
    if path.exists() {
        tracing::info!(
            path = %path.display(),
            "loading JWKS PEM (no bundle yet; previous empty until next rotation)"
        );
        let current = load_keypair_from_pem(path, kid_seed)?;
        // Migrate to bundle so next rotate persists previous.
        persist_atomic(path, &current, None)?;
        return Ok((current, None));
    }
    let current = generate_keypair(kid_seed)?;
    persist_atomic(path, &current, None)?;
    tracing::info!(
        path = %path.display(),
        kid = %current.kid,
        "wrote new JWKS private key bundle"
    );
    Ok((current, None))
}

/// Atomically write bundle JSON + current PEM (G31).
fn persist_atomic(pem_path: &Path, current: &Keypair, previous: Option<&Keypair>) -> Result<()> {
    if let Some(parent) = pem_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create JWKS key dir {}", parent.display()))?;
        }
    }
    let bundle = JwksBundle {
        current_kid: current.kid.clone(),
        current_pem: current.private_pem.clone(),
        previous_kid: previous.map(|p| p.kid.clone()),
        previous_pem: previous.map(|p| p.private_pem.clone()),
    };
    let bundle_file = bundle_path(pem_path);
    let bundle_tmp = bundle_file.with_extension(format!("json.tmp.{}", std::process::id()));
    let text = serde_json::to_string_pretty(&bundle)?;
    std::fs::write(&bundle_tmp, text.as_bytes())
        .with_context(|| format!("write {}", bundle_tmp.display()))?;
    set_secret_perms(&bundle_tmp);
    std::fs::rename(&bundle_tmp, &bundle_file).with_context(|| {
        format!(
            "rename {} -> {}",
            bundle_tmp.display(),
            bundle_file.display()
        )
    })?;
    set_secret_perms(&bundle_file);

    // Also keep a plain PEM at the configured path for operators/openssl.
    let pem_tmp = pem_path.with_extension(format!("pem.tmp.{}", std::process::id()));
    std::fs::write(&pem_tmp, current.private_pem.as_bytes())
        .with_context(|| format!("write {}", pem_tmp.display()))?;
    set_secret_perms(&pem_tmp);
    std::fs::rename(&pem_tmp, pem_path)
        .with_context(|| format!("rename {} -> {}", pem_tmp.display(), pem_path.display()))?;
    set_secret_perms(pem_path);
    Ok(())
}

fn set_secret_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn generate_keypair(kid: &str) -> Result<Keypair> {
    let mut rng = OsRng;
    let priv_key = RsaPrivateKey::new(&mut rng, 2048)?;
    keypair_from_private(priv_key, kid)
}

fn load_keypair_from_pem(path: &Path, kid: &str) -> Result<Keypair> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("read JWKS private key {}", path.display()))?;
    keypair_from_pem_str(&pem, kid)
}

fn keypair_from_pem_str(pem: &str, kid: &str) -> Result<Keypair> {
    let priv_key = RsaPrivateKey::from_pkcs1_pem(pem.trim()).with_context(|| "parse PKCS#1 PEM")?;
    keypair_from_private(priv_key, kid)
}

fn keypair_from_private(priv_key: RsaPrivateKey, kid: &str) -> Result<Keypair> {
    let pub_key = priv_key.to_public_key();
    let pem = priv_key
        .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)?
        .to_string();
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
        private_pem: pem,
    })
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{issue_access_token, verify_access_token};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotate_makes_previous_verifiable() {
        let mgr = JwksManager::new_with_fresh("a").unwrap();
        let initial_kid = mgr.current.load().kid.clone();
        mgr.rotate("b").unwrap();
        assert_eq!(mgr.current.load().kid, "b");
        assert!(mgr.decoding_for(&initial_kid).is_some());
        assert!(mgr.decoding_for("b").is_some());
        assert!(mgr.decoding_for("nope").is_none());
        assert_eq!(mgr.jwks().len(), 2);
    }

    #[test]
    fn persist_survives_reopen_and_verifies_jwt() {
        let dir = tmp_dir("vmcp-jwks-persist");
        let path = dir.join("jwks.pem");

        let mgr1 = JwksManager::open("vmcp", Some(&path)).unwrap();
        let (jwt, _) = issue_access_token(
            &mgr1,
            "https://iss",
            "https://iss/mcp",
            "client",
            "mcp:use",
            3600,
        )
        .unwrap();
        let n1 = mgr1.current.load().jwk.n.clone();

        let mgr2 = JwksManager::open("vmcp", Some(&path)).unwrap();
        assert_eq!(mgr2.current.load().jwk.n, n1, "same key after reopen");
        let claims = verify_access_token(&mgr2, &jwt, "https://iss", &["https://iss/mcp"]).unwrap();
        assert_eq!(claims.client_id, "client");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn previous_key_survives_reopen_after_rotate() {
        let dir = tmp_dir("vmcp-jwks-prev");
        let path = dir.join("jwks.pem");
        let mgr1 = JwksManager::open("vmcp", Some(&path)).unwrap();
        let (jwt_old, _) = issue_access_token(
            &mgr1,
            "https://iss",
            "https://iss/mcp",
            "client",
            "mcp:use",
            3600,
        )
        .unwrap();
        mgr1.rotate("vmcp-2").unwrap();
        assert!(bundle_path(&path).exists());

        let mgr2 = JwksManager::open("vmcp-2", Some(&path)).unwrap();
        assert_eq!(mgr2.jwks().len(), 2);
        let claims =
            verify_access_token(&mgr2, &jwt_old, "https://iss", &["https://iss/mcp"]).unwrap();
        assert_eq!(claims.client_id, "client");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
