//! Static "pre-registered" bearer tokens — eternal, opaque, file-backed.
//!
//! The normal OAuth path issues short-lived RS256 JWTs signed by an EPHEMERAL,
//! rotating JWKS key ([`crate::jwks`]): every restart and every rotation
//! invalidates outstanding tokens. That's correct for browser clients but
//! painful for a demo/CI client that just wants one credential that keeps
//! working across redeploys.
//!
//! A static token is the opposite trade-off: an opaque, high-entropy string
//! (`vmcp_<base64url(32 bytes)>`) generated out-of-band by the `vmcp pre-reg`
//! CLI, stored in a JSON file, and accepted by [`crate::require_bearer`]
//! WITHOUT touching JWKS. It never expires. Revocation = remove its line from
//! the file (the file watcher reloads and the full set is replaced).
//!
//! These are deliberately god-keys: treat the file as a secret. The store
//! itself is watcher-agnostic — the binary wires a `vmcp-watch` callback to
//! [`StaticTokenStore::reload`]; this crate has no filesystem-event dependency.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Prefix marking an opaque static token. A JWT is dot-separated base64url and
/// never starts with this, so the prefix cleanly discriminates the two paths
/// in the bearer middleware.
pub const TOKEN_PREFIX: &str = "vmcp_";

/// One pre-registered token as stored on disk. The file is a JSON array of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticTokenEntry {
    /// The opaque bearer value, `vmcp_<base64url(32 bytes)>`.
    pub token: String,
    /// Stable client identifier attributed to sessions using this token.
    pub client_id: String,
    /// Optional human label (e.g. "ci", "laptop").
    #[serde(default)]
    pub name: Option<String>,
    /// OAuth scope granted to this token.
    pub scope: String,
    /// When the token was generated.
    pub issued_at: DateTime<Utc>,
}

/// In-memory value resolved from a token on a successful lookup.
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub client_id: String,
    pub scope: String,
}

/// Hot-swappable set of static tokens. Wrap in `Arc` and share; the watcher
/// callback calls [`StaticTokenStore::reload`] to replace the set atomically.
pub struct StaticTokenStore {
    set: ArcSwap<HashMap<String, TokenInfo>>,
}

impl StaticTokenStore {
    /// Load the token file into a new store. A missing file is NOT an error —
    /// it yields an empty store (the feature stays inert until tokens exist).
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let map = read_map(path)?;
        let n = map.len();
        let store = Arc::new(Self {
            set: ArcSwap::from_pointee(map),
        });
        tracing::info!(count = n, path = %path.display(), "static token store loaded");
        Ok(store)
    }

    /// Re-read the file and atomically replace the set. On a read/parse error
    /// the current set is KEPT — a half-written file or a typo must not lock
    /// every client out. Removing an entry from the file revokes it on the next
    /// reload, because the whole set is replaced.
    pub fn reload(&self, path: &Path) {
        match read_map(path) {
            Ok(map) => {
                let n = map.len();
                self.set.store(Arc::new(map));
                tracing::info!(count = n, "static token store reloaded");
            }
            Err(e) => {
                tracing::warn!(error = %e, "static token reload failed; keeping previous set");
            }
        }
    }

    /// Resolve a bearer token. `None` for unknown tokens.
    pub fn lookup(&self, token: &str) -> Option<TokenInfo> {
        self.set.load().get(token).cloned()
    }

    /// Number of currently-loaded tokens.
    pub fn len(&self) -> usize {
        self.set.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Read the JSON-array file into a `token -> TokenInfo` map. Missing/empty file
/// -> empty map. Malformed JSON -> error (caller decides whether to keep old).
fn read_map(path: &Path) -> Result<HashMap<String, TokenInfo>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read token file {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let entries: Vec<StaticTokenEntry> = serde_json::from_str(&text)
        .with_context(|| format!("parse token file {}", path.display()))?;
    let mut map = HashMap::with_capacity(entries.len());
    for e in entries {
        map.insert(
            e.token,
            TokenInfo {
                client_id: e.client_id,
                scope: e.scope,
            },
        );
    }
    Ok(map)
}

/// Validate an identifier the way the admin layer does (`validate_id`): ASCII
/// alphanumeric plus `-`/`_`, 1..=128 chars. Keeps `client_id` safe as a
/// recorder session directory name (no path traversal via a crafted file).
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Generate a fresh eternal token entry. `client_id` is derived from `name`
/// (validated); `scope` defaults to `mcp:use`.
pub fn generate_entry(name: &str, scope: Option<&str>) -> Result<StaticTokenEntry> {
    if !valid_id(name) {
        anyhow::bail!("name must be 1..=128 chars of [A-Za-z0-9_-] (used as client_id): {name:?}");
    }
    let mut raw = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    let token = format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    );
    Ok(StaticTokenEntry {
        token,
        client_id: name.to_string(),
        name: Some(name.to_string()),
        scope: scope.unwrap_or("mcp:use").to_string(),
        issued_at: Utc::now(),
    })
}

/// Append `entry` to the JSON-array token file atomically (tmp + rename),
/// creating the parent dir if needed. Reads the existing array first; if it's
/// present but malformed this FAILS LOUDLY rather than clobbering tokens. On
/// unix the file gets `0600` perms (it holds bearer secrets).
///
/// Concurrent `pre-reg` runs are not supported (last writer wins on rename);
/// the CLI is operator-driven and run one at a time.
pub fn append_atomic(path: &Path, entry: &StaticTokenEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create token dir {}", parent.display()))?;
        }
    }

    let mut entries: Vec<StaticTokenEntry> = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))?;
        if text.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "existing token file {} is malformed; refusing to overwrite",
                    path.display()
                )
            })?
        }
    } else {
        Vec::new()
    };
    entries.push(entry.clone());

    let text = serde_json::to_string_pretty(&entries)?;
    // pid-suffixed tmp so a stray concurrent run is less likely to collide.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    set_secret_perms(&tmp);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    set_secret_perms(path);
    Ok(())
}

#[cfg(unix)]
fn set_secret_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_secret_perms(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("vmcp-statictok-test-{nanos}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Overwrite the whole token file with the given entries (atomic), to
    /// simulate hand-edits / removals.
    fn write_all(path: &Path, entries: &[StaticTokenEntry]) {
        let text = serde_json::to_string_pretty(entries).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn generate_entry_is_prefixed_unique_and_validated() {
        let a = generate_entry("ci", Some("mcp:use")).unwrap();
        let b = generate_entry("ci", Some("mcp:use")).unwrap();
        assert!(a.token.starts_with("vmcp_"));
        assert_ne!(a.token, b.token, "tokens must be unique");
        assert_eq!(a.client_id, "ci");
        assert_eq!(a.scope, "mcp:use");
        assert_eq!(
            generate_entry("ci", None).unwrap().scope,
            "mcp:use",
            "scope defaults to mcp:use"
        );
        assert!(
            generate_entry("../etc", None).is_err(),
            "path-y name rejected"
        );
        assert!(generate_entry("", None).is_err(), "empty name rejected");
    }

    #[test]
    fn append_then_load_round_trips() {
        let dir = TempDir::new();
        let f = dir.path().join("tokens.json");

        let e1 = generate_entry("ci", None).unwrap();
        let e2 = generate_entry("laptop", Some("mcp:admin")).unwrap();
        append_atomic(&f, &e1).unwrap();
        append_atomic(&f, &e2).unwrap();

        let store = StaticTokenStore::load(&f).unwrap();
        assert_eq!(store.len(), 2);
        let hit = store.lookup(&e2.token).expect("e2 present");
        assert_eq!(hit.client_id, "laptop");
        assert_eq!(hit.scope, "mcp:admin");
        assert!(store.lookup("vmcp_nope").is_none());
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let dir = TempDir::new();
        let store = StaticTokenStore::load(&dir.path().join("absent.json")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn reload_picks_up_new_token() {
        let dir = TempDir::new();
        let f = dir.path().join("tokens.json");
        let store = StaticTokenStore::load(&f).unwrap(); // empty (no file)
        assert!(store.is_empty());

        let e = generate_entry("ci", None).unwrap();
        append_atomic(&f, &e).unwrap();
        store.reload(&f);
        assert!(
            store.lookup(&e.token).is_some(),
            "reload sees the new token"
        );
    }

    #[test]
    fn malformed_reload_keeps_previous_set() {
        let dir = TempDir::new();
        let f = dir.path().join("tokens.json");
        let e = generate_entry("ci", None).unwrap();
        append_atomic(&f, &e).unwrap();
        let store = StaticTokenStore::load(&f).unwrap();
        assert_eq!(store.len(), 1);

        std::fs::write(&f, b"{ this is not valid json ]").unwrap();
        store.reload(&f);
        assert!(
            store.lookup(&e.token).is_some(),
            "malformed file must not wipe the live set"
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn removing_an_entry_revokes_it_on_reload() {
        let dir = TempDir::new();
        let f = dir.path().join("tokens.json");
        let keep = generate_entry("keep", None).unwrap();
        let revoke = generate_entry("revoke", None).unwrap();
        write_all(&f, &[keep.clone(), revoke.clone()]);
        let store = StaticTokenStore::load(&f).unwrap();
        assert!(store.lookup(&revoke.token).is_some());

        // Operator deletes the revoked line, keeping only `keep`.
        write_all(&f, std::slice::from_ref(&keep));
        store.reload(&f);
        assert!(
            store.lookup(&keep.token).is_some(),
            "kept token still valid"
        );
        assert!(
            store.lookup(&revoke.token).is_none(),
            "removed token is revoked after reload"
        );
    }

    #[test]
    fn append_refuses_to_clobber_malformed_file() {
        let dir = TempDir::new();
        let f = dir.path().join("tokens.json");
        std::fs::write(&f, b"not json").unwrap();
        let e = generate_entry("ci", None).unwrap();
        assert!(
            append_atomic(&f, &e).is_err(),
            "append must fail loudly on a malformed existing file"
        );
    }
}
