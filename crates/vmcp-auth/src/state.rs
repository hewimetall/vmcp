//! Process-local auth state. Single source of truth shared by all OAuth
//! endpoints + the bearer middleware.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::jwks::JwksManager;
use crate::types::{AuthCodeRecord, ClientInfo, ConsentSession};

/// Auth configuration extracted from [`vmcp_config::AuthConfig`] + public URL.
#[derive(Clone)]
pub struct AuthState {
    pub jwks: Arc<JwksManager>,
    pub issuer: String,
    /// Canonical resource indicator for the MCP endpoint (e.g. https://host/mcp).
    pub resource_audience: String,
    pub token_ttl_secs: u64,
    /// PHC-encoded master password hash.
    pub master_password_hash: String,
    /// Default scope granted on successful consent.
    pub default_scope: String,

    pub clients: Arc<DashMap<String, ClientInfo>>,
    pub codes: Arc<DashMap<String, AuthCodeRecord>>,
    pub consents: Arc<DashMap<String, ConsentSession>>,
}

impl AuthState {
    pub fn new(
        jwks: Arc<JwksManager>,
        issuer: impl Into<String>,
        resource_audience: impl Into<String>,
        token_ttl_secs: u64,
        master_password_hash: impl Into<String>,
    ) -> Self {
        Self {
            jwks,
            issuer: issuer.into(),
            resource_audience: resource_audience.into(),
            token_ttl_secs,
            master_password_hash: master_password_hash.into(),
            default_scope: "mcp:use".into(),
            clients: Arc::new(DashMap::new()),
            codes: Arc::new(DashMap::new()),
            consents: Arc::new(DashMap::new()),
        }
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
}
