//! Process-local auth state shared by all OAuth endpoints + the bearer middleware.
//!
//! DCR clients are cached in [`Self::clients`] and durably stored in
//! [`Self::client_store`] (SQLite, same idea as TaskStore). Auth codes and
//! consent sessions stay ephemeral — a restart invalidates in-flight OAuth.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tracing::info;

use crate::client_store::{ClientStore, ClientStoreError};
use crate::jwks::JwksManager;
use crate::static_tokens::StaticTokenStore;
use crate::types::{
    next_unique_name, slugify_client_base, valid_client_display_name, AuthCodeRecord, ClientInfo,
    ConsentSession,
};

/// Runtime auth state built from gateway config (`auth.*` + public URL).
#[derive(Clone)]
pub struct AuthState {
    pub jwks: Arc<JwksManager>,
    pub issuer: String,
    /// Primary resource indicator (e.g. `https://host/mcp`) — advertised in
    /// bare protected-resource metadata.
    pub resource_audience: String,
    /// All accepted RFC 8707 resource URLs for this gateway (`/mcp`,
    /// `/mcp-proxy`, …). Bearer JWTs may use any of these as `aud`.
    pub resource_audiences: Vec<String>,
    pub token_ttl_secs: u64,
    /// PHC-encoded master password hash.
    pub master_password_hash: String,
    /// Default scope granted on successful consent.
    pub default_scope: String,

    /// Hot cache of DCR clients. Hydrated from [`Self::client_store`] at boot;
    /// write-through on `POST /register`.
    pub clients: Arc<DashMap<String, ClientInfo>>,
    pub codes: Arc<DashMap<String, AuthCodeRecord>>,
    pub consents: Arc<DashMap<String, ConsentSession>>,

    /// Durable DCR client registry (`auth.clients_db_path`). `None` only when
    /// auth is disabled / store was not attached.
    pub client_store: Option<Arc<ClientStore>>,

    /// Optional pre-registered static (eternal, opaque) token store, populated
    /// from `auth.tokens_file`. `None` when the key is unset — the OAuth/JWT
    /// flow is then the only way onto `/mcp`. Wrapped in `Arc` so `AuthState`
    /// stays `Clone` (the inner `ArcSwap` is not `Clone`).
    pub token_store: Option<Arc<StaticTokenStore>>,

    /// Dynamic Client Registration policy (from `auth.dcr_*` config).
    pub dcr: DcrPolicy,

    /// Serializes `POST /register` so `dcr_max_clients` checks are atomic (G28).
    pub dcr_register_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Limits for `POST /register`. Defaults match historical open registration.
#[derive(Debug, Clone)]
pub struct DcrPolicy {
    pub enabled: bool,
    /// `0` = unlimited.
    pub max_clients: u64,
    /// Prefix allowlist; empty = allow any redirect_uri.
    pub redirect_uri_allowlist: Vec<String>,
}

impl Default for DcrPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_clients: 0,
            redirect_uri_allowlist: Vec::new(),
        }
    }
}

impl DcrPolicy {
    /// Whether `uri` is permitted by the allowlist.
    ///
    /// Entries that parse as URLs match on **exact host** + scheme. Port is
    /// checked only when the allowlist entry specifies one. Path on the
    /// allowlist entry (if not `/`) is a required prefix. Custom-scheme
    /// prefixes like `cursor://` still use starts-with. Rejects
    /// `http://127.0.0.1.evil/...` when allowlist has `http://127.0.0.1` (G27).
    pub fn redirect_uri_allowed(&self, uri: &str) -> bool {
        if self.redirect_uri_allowlist.is_empty() {
            return true;
        }
        let Ok(parsed) = url::Url::parse(uri) else {
            return false;
        };
        self.redirect_uri_allowlist.iter().any(|entry| {
            if entry.is_empty() {
                return false;
            }
            if let Ok(allowed) = url::Url::parse(entry) {
                // `cursor://` parses with empty host — treat as scheme prefix.
                if allowed.host_str().is_none() {
                    return uri.starts_with(entry.as_str());
                }
                if parsed.scheme() != allowed.scheme() {
                    return false;
                }
                if parsed.host_str() != allowed.host_str() {
                    return false;
                }
                // Explicit port on allowlist → must match; otherwise any port OK.
                if let Some(p) = allowed.port() {
                    if parsed.port() != Some(p) {
                        return false;
                    }
                }
                let allow_path = allowed.path();
                if allow_path.is_empty() || allow_path == "/" {
                    return true;
                }
                let path = parsed.path();
                return path == allow_path
                    || path.starts_with(&format!("{}/", allow_path.trim_end_matches('/')));
            }
            // Unparseable allow entries: literal prefix.
            uri.starts_with(entry.as_str())
        })
    }
}

impl AuthState {
    pub fn new(
        jwks: Arc<JwksManager>,
        issuer: impl Into<String>,
        resource_audience: impl Into<String>,
        token_ttl_secs: u64,
        master_password_hash: impl Into<String>,
    ) -> Self {
        let resource_audience = resource_audience.into();
        Self {
            jwks,
            issuer: issuer.into(),
            resource_audiences: vec![resource_audience.clone()],
            resource_audience,
            token_ttl_secs,
            master_password_hash: master_password_hash.into(),
            default_scope: "mcp:use".into(),
            clients: Arc::new(DashMap::new()),
            codes: Arc::new(DashMap::new()),
            consents: Arc::new(DashMap::new()),
            client_store: None,
            token_store: None,
            dcr: DcrPolicy::default(),
            dcr_register_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn with_dcr_policy(mut self, dcr: DcrPolicy) -> Self {
        self.dcr = dcr;
        self
    }

    /// Attach a durable DCR client store and hydrate the in-memory cache.
    pub fn with_client_store(mut self, store: Arc<ClientStore>) -> Result<Self, AuthStateError> {
        let rows = store
            .list()
            .map_err(|source| AuthStateError::ClientStoreHydration {
                path: store.path().to_path_buf(),
                source,
            })?;
        info!(
            path = %store.path().display(),
            count = rows.len(),
            "loaded DCR clients from SQLite"
        );
        for c in rows {
            self.clients.insert(c.client_id.clone(), c);
        }
        self.client_store = Some(store);
        Ok(self)
    }

    /// Extra MCP mount URLs that may appear as OAuth `resource` / JWT `aud`
    /// (e.g. `https://host/mcp-proxy` alongside the primary `/mcp`).
    pub fn with_extra_resource_audiences(
        mut self,
        extra: impl IntoIterator<Item = String>,
    ) -> Self {
        for a in extra {
            if !self.resource_audiences.iter().any(|x| x == &a) {
                self.resource_audiences.push(a);
            }
        }
        self
    }

    /// Slice of accepted audiences for JWT verification.
    pub fn audience_refs(&self) -> Vec<&str> {
        self.resource_audiences.iter().map(String::as_str).collect()
    }

    /// Attach a pre-registered static token store. Builder form so the `new`
    /// signature — and every existing call site — stays untouched.
    pub fn with_token_store(mut self, store: Arc<StaticTokenStore>) -> Self {
        self.token_store = Some(store);
        self
    }

    /// Purge codes and consent sessions older than `max_age`.
    pub fn gc(&self, max_age: Duration) {
        let cutoff = chrono::Utc::now() - chrono::Duration::from_std(max_age).unwrap();
        self.codes.retain(|_, v| v.issued_at > cutoff);
        self.consents.retain(|_, v| v.created_at > cutoff);
    }

    /// Snapshot of all currently-registered OAuth dynamic clients.
    pub fn list_clients(&self) -> Vec<crate::types::ClientInfo> {
        self.clients.iter().map(|r| r.value().clone()).collect()
    }

    /// Auto-generate a unique operator `name` from an optional RFC `client_name`.
    pub fn allocate_client_name(&self, client_name: Option<&str>) -> String {
        let base = slugify_client_base(client_name);
        let existing: std::collections::HashSet<String> =
            self.clients.iter().map(|r| r.name.clone()).collect();
        next_unique_name(&existing, &base)
    }

    /// Rename a DCR client (admin Sessions UI). Writes through to SQLite when present.
    pub fn rename_client(
        &self,
        client_id: &str,
        name: &str,
    ) -> Result<ClientInfo, RenameClientError> {
        if !valid_client_display_name(name) {
            return Err(RenameClientError::InvalidName(name.into()));
        }
        let taken = self
            .clients
            .iter()
            .any(|r| r.name == name && r.client_id != client_id);
        if taken {
            return Err(RenameClientError::NameTaken(name.into()));
        }

        let mut info = self
            .clients
            .get(client_id)
            .map(|r| r.value().clone())
            .ok_or_else(|| RenameClientError::NotFound(client_id.into()))?;
        info.name = name.to_string();

        if let Some(store) = self.client_store.as_ref() {
            store.set_name(client_id, name)?;
        }
        self.clients.insert(client_id.to_string(), info.clone());
        Ok(info)
    }
}

/// Errors from [`AuthState`] construction and hydration.
#[derive(Debug, thiserror::Error)]
pub enum AuthStateError {
    #[error(
        "failed to hydrate DCR clients from SQLite at {path}; clients.db may contain corrupt client JSON or unreadable rows; delete auth.clients_db_path and re-register clients: {source}"
    )]
    ClientStoreHydration {
        path: PathBuf,
        #[source]
        source: ClientStoreError,
    },
}

/// Errors from [`AuthState::rename_client`].
#[derive(Debug, thiserror::Error)]
pub enum RenameClientError {
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("name already taken: {0}")]
    NameTaken(String),
    #[error("client not found: {0}")]
    NotFound(String),
    #[error("store: {0}")]
    Store(#[from] ClientStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_store::ClientStore;
    use crate::jwks::JwksManager;
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn with_client_store_hydrates_dashmap_across_reopen() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("clients.db");
        {
            let store = ClientStore::open(&db).unwrap();
            store
                .upsert(&ClientInfo {
                    client_id: "vmcp-cursor".into(),
                    redirect_uris: vec!["http://127.0.0.1/cb".into()],
                    client_name: Some("Cursor".into()),
                    name: "cursor".into(),
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    scope: Some("mcp:use".into()),
                    issued_at: Utc::now(),
                })
                .unwrap();
        }

        let jwks = JwksManager::new_with_fresh("test").unwrap();
        let state = AuthState::new(jwks, "http://issuer", "http://issuer/mcp", 3600, "hash")
            .with_client_store(Arc::new(ClientStore::open(&db).unwrap()))
            .unwrap();

        assert!(state.clients.contains_key("vmcp-cursor"));
        assert_eq!(state.list_clients().len(), 1);
        assert_eq!(state.clients.get("vmcp-cursor").unwrap().name, "cursor");
        assert!(state.client_store.is_some());

        let renamed = state.rename_client("vmcp-cursor", "laptop").unwrap();
        assert_eq!(renamed.name, "laptop");
        assert_eq!(state.allocate_client_name(Some("Cursor")), "cursor");
    }

    #[test]
    fn with_client_store_fails_fast_on_corrupt_client_json() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("clients.db");
        drop(ClientStore::open(&db).unwrap());
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO oauth_clients (
                    client_id, redirect_uris_json, client_name, name,
                    grant_types_json, response_types_json, scope, issued_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    "vmcp-bad-json",
                    "not-json",
                    "Cursor",
                    "cursor",
                    r#"["authorization_code"]"#,
                    r#"["code"]"#,
                    "mcp:use",
                    Utc::now().timestamp_millis(),
                ],
            )
            .unwrap();
        }

        let jwks = JwksManager::new_with_fresh("test").unwrap();
        let err = match AuthState::new(jwks, "http://issuer", "http://issuer/mcp", 3600, "hash")
            .with_client_store(Arc::new(ClientStore::open(&db).unwrap()))
        {
            Ok(_) => panic!("corrupt DCR cache must fail boot"),
            Err(err) => err,
        };

        let msg = err.to_string();
        assert!(
            msg.contains("failed to hydrate DCR clients from SQLite"),
            "error should name the hydration failure: {msg}"
        );
        assert!(
            msg.contains("delete auth.clients_db_path and re-register clients"),
            "error should tell operators how to recover: {msg}"
        );
        assert!(
            msg.contains(db.to_string_lossy().as_ref()),
            "error should include the db path: {msg}"
        );
    }

    #[test]
    fn dcr_redirect_allowlist_host_match_not_prefix_host() {
        let open = DcrPolicy::default();
        assert!(open.redirect_uri_allowed("https://evil.example/cb"));

        let locked = DcrPolicy {
            enabled: true,
            max_clients: 10,
            redirect_uri_allowlist: vec!["http://127.0.0.1".into(), "cursor://".into()],
        };
        assert!(locked.redirect_uri_allowed("http://127.0.0.1:9999/cb"));
        assert!(locked.redirect_uri_allowed("cursor://anysphere.cursor-mcp/oauth/callback"));
        assert!(!locked.redirect_uri_allowed("https://evil.example/cb"));
        // Prefix-host attack must fail (G27).
        assert!(!locked.redirect_uri_allowed("http://127.0.0.1.evil.example/cb"));
    }

    #[test]
    fn dcr_allowlist_path_port_and_unparseable_entries() {
        let with_path = DcrPolicy {
            enabled: true,
            max_clients: 0,
            redirect_uri_allowlist: vec![
                "https://app.example/callback".into(),
                "http://127.0.0.1:8080".into(),
                "".into(),
                "custom-scheme:prefix".into(),
            ],
        };
        assert!(with_path.redirect_uri_allowed("https://app.example/callback"));
        assert!(with_path.redirect_uri_allowed("https://app.example/callback/extra"));
        assert!(!with_path.redirect_uri_allowed("https://app.example/other"));
        assert!(with_path.redirect_uri_allowed("http://127.0.0.1:8080/x"));
        assert!(!with_path.redirect_uri_allowed("http://127.0.0.1:9999/x"));
        // Unparseable allow entry → literal prefix match.
        assert!(with_path.redirect_uri_allowed("custom-scheme:prefix/more"));
        assert!(!with_path.redirect_uri_allowed("://broken"));
    }

    #[test]
    fn gc_and_audience_helpers() {
        use crate::types::{AuthCodeRecord, ConsentSession};

        let jwks = JwksManager::new_with_fresh("gc").unwrap();
        let state = AuthState::new(jwks, "https://iss", "https://iss/mcp", 3600, "hash")
            .with_extra_resource_audiences(vec![
                "https://iss/mcp".into(),
                "https://iss/mcp-proxy".into(),
            ]);
        assert_eq!(state.audience_refs().len(), 2);

        let old = Utc::now() - chrono::Duration::hours(2);
        state.codes.insert(
            "old".into(),
            AuthCodeRecord {
                code: "old".into(),
                client_id: "c".into(),
                redirect_uri: "http://127.0.0.1/cb".into(),
                code_challenge: "x".into(),
                code_challenge_method: "S256".into(),
                scope: "mcp:use".into(),
                resource: None,
                issued_at: old,
            },
        );
        state.consents.insert(
            "cs".into(),
            ConsentSession {
                id: "cs".into(),
                client_id: "c".into(),
                redirect_uri: "http://127.0.0.1/cb".into(),
                state: None,
                scope: "mcp:use".into(),
                code_challenge: "x".into(),
                code_challenge_method: "S256".into(),
                resource: None,
                created_at: old,
            },
        );
        state.gc(Duration::from_secs(60));
        assert!(state.codes.is_empty());
        assert!(state.consents.is_empty());
    }
}
