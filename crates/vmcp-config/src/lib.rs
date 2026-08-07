//! vmcp configuration: TOML on disk + env-var overrides.
//!
//! Replaces Python `vmcp/config.py`. Single source of truth for ports,
//! registry/lock paths, GraphQL limits, upstream timeouts, OAuth knobs.
//! Env vars use the `VMCP_` prefix and `__` as the nested separator
//! (`figment` default).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Top-level vmcp configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    /// IP to bind the HTTP listener to.
    #[serde(default = "Settings::default_host")]
    pub host: IpAddr,

    /// Port to bind. 8765 is the demo default.
    #[serde(default = "Settings::default_port")]
    pub port: u16,

    /// Path the rmcp `StreamableHttpService` is mounted under.
    #[serde(default = "Settings::default_mcp_path")]
    pub mcp_path: String,

    /// Public URL the gateway is reachable at (https://... in prod). Used as
    /// the OAuth `issuer` and in resource-server metadata.
    #[serde(default = "Settings::default_public_base_url")]
    pub public_base_url: String,

    /// Path to the upstream registry JSON.
    #[serde(default = "Settings::default_registry_path")]
    pub registry_path: PathBuf,

    /// Path to the tools.lock JSON.
    #[serde(default = "Settings::default_lock_path")]
    pub lock_path: PathBuf,

    /// Directory with sidecar specs (`specs/<server>.json`).
    #[serde(default = "Settings::default_spec_dir")]
    pub spec_dir: PathBuf,

    /// Directory with operator-authored skill YAMLs. Each `*.yaml` becomes
    /// an MCP prompt accessible via `prompts/list` / `prompts/get`. Missing
    /// directory is fine — vmcp boots with zero skills.
    #[serde(default = "Settings::default_skills_dir")]
    pub skills_dir: PathBuf,

    /// Max retained notifications in the in-memory bus ring buffer.
    #[serde(default = "Settings::default_notif_ring_max")]
    pub notif_ring_max: usize,

    #[serde(default)]
    pub gql: GqlConfig,

    #[serde(default)]
    pub upstream: UpstreamConfig,

    #[serde(default)]
    pub auth: AuthConfig,

    #[serde(default)]
    pub recorder: RecorderCfg,

    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Native MCP Tasks (SEP-1686). Off by default; when enabled, vmcp registers
    /// `run_task` for upstream tools with `execution.taskSupport` (or sidecar
    /// `task_support`) and advertises the server `tasks` capability. Durable
    /// task state lives in SQLite (`db_path`), same idea as mcp-presentation.
    #[serde(default)]
    pub tasks: TasksConfig,
}

/// Native MCP Tasks / `run_task` integration (SEP-1686).
///
/// When `enabled = true` and at least one upstream tool is task-capable, vmcp
/// registers `run_task` (`execution.taskSupport = optional`) and declares the
/// server `tasks` capability. Task rows persist in embedded SQLite.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TasksConfig {
    /// Master switch. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// SQLite database path for durable task rows (WAL).
    #[serde(default = "TasksConfig::default_db_path")]
    pub db_path: PathBuf,
    /// Default task retention (ttl) in milliseconds advertised on creation.
    #[serde(default = "TasksConfig::default_task_ttl_ms")]
    pub task_ttl_ms: u64,
    /// Suggested client poll interval (ms) returned on tasks.
    #[serde(default = "TasksConfig::default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Max concurrently-running tasks.
    #[serde(default = "TasksConfig::default_max_concurrent")]
    pub max_concurrent: usize,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: Self::default_db_path(),
            task_ttl_ms: Self::default_task_ttl_ms(),
            poll_interval_ms: Self::default_poll_interval_ms(),
            max_concurrent: Self::default_max_concurrent(),
        }
    }
}

impl TasksConfig {
    fn default_db_path() -> PathBuf {
        PathBuf::from("state/tasks.db")
    }
    fn default_task_ttl_ms() -> u64 {
        300_000
    }
    fn default_poll_interval_ms() -> u64 {
        2_000
    }
    fn default_max_concurrent() -> usize {
        16
    }
}

/// Transparent MCP proxy on a side endpoint. When `enabled`, vmcp mounts a
/// second `StreamableHttpService` at `mcp_path` that exposes upstream tools
/// and prompts 1:1 (prefixed `{server}__{name}`) instead of the GraphQL
/// semantic layer. Upstream `prompts/get` responses are prepended with a
/// GraphQL tool-routing table. The main `/mcp` endpoint keeps serving GraphQL
/// (including upstream prompts when this flag is on) — both run in the same
/// process behind the same OAuth bearer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "ProxyConfig::default_mcp_path")]
    pub mcp_path: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcp_path: Self::default_mcp_path(),
        }
    }
}

impl ProxyConfig {
    fn default_mcp_path() -> String {
        "/mcp-proxy".into()
    }
}

/// Session recorder configuration — where on-disk replay logs land and how
/// they're scrubbed before being persisted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecorderCfg {
    #[serde(default = "default_sessions_dir")]
    pub sessions_dir: std::path::PathBuf,
    #[serde(default = "default_redact_keys")]
    pub redact_keys: Vec<String>,
    /// Mark admin-registry sessions idle after this many seconds without
    /// traffic, and tear down the matching rmcp transport session (releases
    /// SSE channels / FDs). Also used as rmcp `SessionConfig::keep_alive`.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
    #[serde(default = "default_gc_interval")]
    pub gc_interval_secs: u64,
}

fn default_sessions_dir() -> std::path::PathBuf {
    "./sessions".into()
}
fn default_redact_keys() -> Vec<String> {
    vec![
        "password".into(),
        "secret".into(),
        "token".into(),
        "api_key".into(),
        "Authorization".into(),
    ]
}
fn default_idle_ttl() -> u64 {
    300
}
fn default_gc_interval() -> u64 {
    30
}

impl Default for RecorderCfg {
    fn default() -> Self {
        Self {
            sessions_dir: default_sessions_dir(),
            redact_keys: default_redact_keys(),
            idle_ttl_secs: default_idle_ttl(),
            gc_interval_secs: default_gc_interval(),
        }
    }
}

/// Behaviour when an upstream tool response exceeds `max_response_bytes`.
///
/// `Error` rejects the call with `isError=true` and structured metadata in
/// `json` (no payload — prevents oversized responses from corrupting agent
/// context).
/// `Truncate` returns the first N bytes verbatim in `text` and also exposes
/// a `_data_prefix` field on `json` for programmatic consumers.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CapMode {
    #[default]
    Error,
    Truncate,
}

/// GraphQL semantic-layer limits.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GqlConfig {
    pub max_depth: usize,
    pub max_complexity: usize,
    /// Hard cap on the size (bytes) of the upstream `text` payload before we
    /// either error or truncate. Anything over the cap is replaced with a
    /// summary node — agents see structured metadata instead of a 1.4 MB blob
    /// nuking their context window.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default)]
    pub response_cap_mode: CapMode,
}

fn default_max_response_bytes() -> usize {
    1_048_576 // 1 MiB
}

impl Default for GqlConfig {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_complexity: 1000,
            max_response_bytes: default_max_response_bytes(),
            response_cap_mode: CapMode::default(),
        }
    }
}

/// Upstream stdio pool timing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub spawn_timeout_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            spawn_timeout_ms: 30_000,
            call_timeout_ms: 60_000,
        }
    }
}

/// Which auth backend the facade mounts.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthProviderKind {
    /// Built-in OAuth 2.1 AS + RS (DCR, PKCE, local JWKS, master-password consent).
    #[default]
    Local,
    /// External Authentik IdP: Bearer JWT + optional forward-auth headers.
    Authentik,
}

/// How `/admin` authenticates operators.
///
/// Independent of the MCP facade (`provider`): admin may be open, HTTP Basic,
/// or Authentik forward-auth headers.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdminAuthMode {
    /// No auth on `/admin` (local/dev only).
    None,
    /// HTTP Basic (`user:password`) against `master_password_argon2`.
    #[default]
    Basic,
    /// Trust gateway `X-authentik-*` headers (exact group membership).
    Authentik,
}

/// Settings for the `/admin` SPA auth gate (`[auth.admin]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminAuthSettings {
    /// `none` | `basic` (default) | `authentik`.
    #[serde(default)]
    pub mode: AdminAuthMode,
    /// Exact Authentik groups that may open `/admin` when `mode = authentik`.
    /// Matching uses delimiter-split + exact equality (never substring).
    #[serde(default)]
    pub required_groups: Vec<String>,
    /// Override username header (default: `[auth.authentik].username_header`).
    #[serde(default)]
    pub username_header: Option<String>,
    /// Override groups header (default: `[auth.authentik].groups_header`).
    #[serde(default)]
    pub groups_header: Option<String>,
}

impl Default for AdminAuthSettings {
    fn default() -> Self {
        Self {
            mode: AdminAuthMode::Basic,
            required_groups: Vec::new(),
            username_header: None,
            groups_header: None,
        }
    }
}

/// Authentik-specific settings (`[auth.authentik]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthentikSettings {
    /// OIDC issuer URL (with trailing slash), e.g.
    /// `https://auth.example.com/application/o/mcp-internal/`.
    #[serde(default)]
    pub issuer: String,
    /// JWKS URL for signature verification.
    #[serde(default)]
    pub jwks_url: String,
    /// Accepted JWT `aud` values (resource indicators). Empty → derived from
    /// `public_base_url` + mcp/proxy paths at boot.
    #[serde(default)]
    pub audiences: Vec<String>,
    /// Accept `Authorization: Bearer` JWTs from Authentik (MCP clients).
    #[serde(default = "AuthentikSettings::default_true")]
    pub accept_bearer: bool,
    /// Accept `X-authentik-*` headers from a trusted forward-auth gateway.
    #[serde(default = "AuthentikSettings::default_true")]
    pub forward_auth: bool,
    /// Username header (default `x-authentik-username`).
    #[serde(default = "AuthentikSettings::default_username_header")]
    pub username_header: String,
    /// Groups header (default `x-authentik-groups`).
    #[serde(default = "AuthentikSettings::default_groups_header")]
    pub groups_header: String,
    /// JWT claim carrying groups (default `groups`).
    #[serde(default = "AuthentikSettings::default_groups_claim")]
    pub groups_claim: String,
    /// Exact Authentik group → space-separated MCP scopes.
    /// Matching is exact after delimiter split (`|`, `,`, `;`, space).
    #[serde(default)]
    pub group_scopes: std::collections::BTreeMap<String, String>,
    /// TCP peer CIDRs allowed to present `X-authentik-*` (Gateway / mesh).
    /// Required (alone or with `forward_auth_secret`) when `forward_auth = true`.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Shared hop secret the authenticating proxy injects. Prefer env
    /// `VMCP_AUTH__AUTHENTIK__FORWARD_AUTH_SECRET`.
    #[serde(default)]
    pub forward_auth_secret: String,
    /// Header carrying [`Self::forward_auth_secret`].
    #[serde(default = "AuthentikSettings::default_forward_auth_secret_header")]
    pub forward_auth_secret_header: String,
}

impl AuthentikSettings {
    fn default_true() -> bool {
        true
    }
    fn default_username_header() -> String {
        "x-authentik-username".into()
    }
    fn default_groups_header() -> String {
        "x-authentik-groups".into()
    }
    fn default_groups_claim() -> String {
        "groups".into()
    }
    fn default_forward_auth_secret_header() -> String {
        "x-vmcp-forward-auth".into()
    }
}

impl Default for AuthentikSettings {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            jwks_url: String::new(),
            audiences: Vec::new(),
            accept_bearer: true,
            forward_auth: true,
            username_header: Self::default_username_header(),
            groups_header: Self::default_groups_header(),
            groups_claim: Self::default_groups_claim(),
            group_scopes: std::collections::BTreeMap::new(),
            trusted_proxies: Vec::new(),
            forward_auth_secret: String::new(),
            forward_auth_secret_header: Self::default_forward_auth_secret_header(),
        }
    }
}

/// OAuth 2.1 AS + RS configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Whether OAuth bearer enforcement on `/mcp` (and proxy + admin) is on.
    /// When `false`, the bearer middleware is not mounted and `/admin` is not
    /// nested — intended for local dev clusters only.
    #[serde(default = "AuthConfig::default_enabled")]
    pub enabled: bool,
    /// Auth facade backend: `local` (default) or `authentik`.
    #[serde(default)]
    pub provider: AuthProviderKind,
    /// argon2id-encoded master password (generate via `vmcp hash-password`).
    /// Required when `enabled = true` and `provider = local`; optional for
    /// Authentik (only needed if `/admin` Basic auth should stay available).
    #[serde(default)]
    pub master_password_argon2: String,
    /// JWT key ID exposed via JWKS.
    #[serde(default = "AuthConfig::default_jwt_kid")]
    pub jwt_kid: String,
    /// JWKS rotation interval. Two-key window (current + previous) covers
    /// tokens issued just before rotation. Must be >= 2 * token_ttl_secs
    /// when `enabled = true` and `provider = local`.
    #[serde(default = "AuthConfig::default_jwks_rotate_secs")]
    pub jwks_rotate_secs: u64,
    /// Access-token TTL.
    #[serde(default = "AuthConfig::default_token_ttl_secs")]
    pub token_ttl_secs: u64,
    /// OAuth issuer URL. Defaults to `public_base_url` if not set.
    #[serde(default)]
    pub issuer: Option<String>,
    /// Optional path to a JSON file of pre-registered static (eternal, opaque)
    /// bearer tokens. Generate entries with `vmcp pre-reg`. When set, the file
    /// is loaded at boot and hot-reloaded on change; its `vmcp_`-prefixed
    /// tokens bypass the OAuth/JWT flow. Unset (`None`) keeps OAuth as the only
    /// way in. Override via `VMCP_AUTH__TOKENS_FILE`.
    #[serde(default)]
    pub tokens_file: Option<PathBuf>,
    /// SQLite path for durable DCR (RFC 7591) client registrations. Survives
    /// gateway restart so Cursor can keep reusing its stored `client_id`.
    /// Same idea as `[tasks].db_path`. Override via `VMCP_AUTH__CLIENTS_DB_PATH`.
    #[serde(default = "AuthConfig::default_clients_db_path")]
    pub clients_db_path: PathBuf,
    /// When false, `POST /register` returns 403 (pre-reg / static tokens still work).
    #[serde(default = "AuthConfig::default_dcr_enabled")]
    pub dcr_enabled: bool,
    /// Max durable DCR clients (`0` = unlimited). Excess registrations → 403.
    #[serde(default)]
    pub dcr_max_clients: u64,
    /// If non-empty, each `redirect_uri` must start with one of these prefixes
    /// (e.g. `http://127.0.0.1`, `cursor://`). Empty = allow any URI (legacy).
    #[serde(default)]
    pub dcr_redirect_uri_allowlist: Vec<String>,
    /// Optional PKCS#1 PEM path for the JWT signing key. When set, the key is
    /// loaded at boot (or generated + written `0600` if missing) so access
    /// tokens survive restarts. Unset = ephemeral in-memory JWKS (historical).
    /// Override via `VMCP_AUTH__JWKS_PRIVATE_KEY_PEM_PATH`.
    #[serde(default)]
    pub jwks_private_key_pem_path: Option<PathBuf>,
    /// Authentik settings when `provider = "authentik"`.
    #[serde(default)]
    pub authentik: AuthentikSettings,
    /// `/admin` SPA auth: `none` | `basic` | `authentik` headers.
    #[serde(default)]
    pub admin: AdminAuthSettings,
}

impl AuthConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_jwt_kid() -> String {
        "vmcp".into()
    }
    fn default_jwks_rotate_secs() -> u64 {
        86_400
    }
    fn default_token_ttl_secs() -> u64 {
        3_600
    }
    fn default_clients_db_path() -> PathBuf {
        PathBuf::from("state/clients.db")
    }
    fn default_dcr_enabled() -> bool {
        true
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            provider: AuthProviderKind::Local,
            master_password_argon2: String::new(),
            jwt_kid: Self::default_jwt_kid(),
            jwks_rotate_secs: Self::default_jwks_rotate_secs(),
            token_ttl_secs: Self::default_token_ttl_secs(),
            issuer: None,
            tokens_file: None,
            clients_db_path: Self::default_clients_db_path(),
            dcr_enabled: Self::default_dcr_enabled(),
            dcr_max_clients: 0,
            dcr_redirect_uri_allowlist: Vec::new(),
            jwks_private_key_pem_path: None,
            authentik: AuthentikSettings::default(),
            admin: AdminAuthSettings::default(),
        }
    }
}

impl Settings {
    fn default_host() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }
    fn default_port() -> u16 {
        8765
    }
    fn default_mcp_path() -> String {
        "/mcp".into()
    }
    fn default_public_base_url() -> String {
        "http://localhost:8765".into()
    }
    fn default_registry_path() -> PathBuf {
        "./registry.json".into()
    }
    fn default_lock_path() -> PathBuf {
        "./tools.lock.json".into()
    }
    fn default_spec_dir() -> PathBuf {
        "./specs".into()
    }
    fn default_skills_dir() -> PathBuf {
        "./skills".into()
    }
    fn default_notif_ring_max() -> usize {
        10_000
    }

    /// Effective OAuth issuer (auth.issuer override or public_base_url).
    pub fn effective_issuer(&self) -> &str {
        self.auth.issuer.as_deref().unwrap_or(&self.public_base_url)
    }

    /// Validate cross-field invariants for HTTP ingress. Call after `load`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_common()?;
        // When auth.enabled = false the bearer middleware is not mounted, so
        // the password hash and JWKS rotation window are unused. Skip both
        // checks — operators can leave master_password_argon2 empty.
        if self.auth.enabled {
            match self.auth.provider {
                AuthProviderKind::Local => self.validate_local_auth()?,
                AuthProviderKind::Authentik => self.validate_authentik_auth()?,
            }
            self.validate_admin_auth()?;
        }
        Ok(())
    }

    fn validate_master_password_hash(&self) -> Result<(), ConfigError> {
        if self.auth.master_password_argon2.is_empty() {
            return Err(ConfigError::Validation(
                "auth.master_password_argon2 is required".into(),
            ));
        }
        if !self.auth.master_password_argon2.starts_with("$argon2") {
            return Err(ConfigError::Validation(
                "auth.master_password_argon2 must be a PHC-encoded argon2 hash".into(),
            ));
        }
        // Actually parse the PHC string. The prefix check above passes for
        // the shipped placeholder (`…$REPLACE_ME`), whose trailing segment
        // is not valid Base64 — that slips through boot and only blows up at
        // the OAuth `/consent` step with a cryptic "invalid Base64 encoding".
        // Fail fast here with an actionable message instead.
        if let Err(e) = argon2::password_hash::PasswordHash::new(&self.auth.master_password_argon2)
        {
            return Err(ConfigError::Validation(format!(
                "auth.master_password_argon2 is not a valid argon2 hash ({e}) — \
                 generate one with `vmcp hash-password` and set it via \
                 VMCP_AUTH__MASTER_PASSWORD_ARGON2 or the [auth] block"
            )));
        }
        Ok(())
    }

    fn validate_local_auth(&self) -> Result<(), ConfigError> {
        // Local AS consent always needs the master password.
        self.validate_master_password_hash()?;
        // Token TTL must fit in the rotation window: clients issued just before
        // a rotation should still verify against `previous`.
        if self.auth.jwks_rotate_secs < 2 * self.auth.token_ttl_secs {
            return Err(ConfigError::Validation(format!(
                "auth.jwks_rotate_secs ({}) must be >= 2 * token_ttl_secs ({})",
                self.auth.jwks_rotate_secs, self.auth.token_ttl_secs
            )));
        }
        if self.auth.clients_db_path.as_os_str().is_empty() {
            return Err(ConfigError::Validation(
                "auth.clients_db_path must be set when auth.enabled = true".into(),
            ));
        }
        Ok(())
    }

    fn validate_authentik_auth(&self) -> Result<(), ConfigError> {
        let ak = &self.auth.authentik;
        if ak.issuer.is_empty() {
            return Err(ConfigError::Validation(
                "auth.authentik.issuer is required when auth.provider = authentik".into(),
            ));
        }
        if ak.jwks_url.is_empty() {
            return Err(ConfigError::Validation(
                "auth.authentik.jwks_url is required when auth.provider = authentik".into(),
            ));
        }
        if !ak.accept_bearer && !ak.forward_auth {
            return Err(ConfigError::Validation(
                "auth.authentik: at least one of accept_bearer or forward_auth must be true".into(),
            ));
        }
        if ak.forward_auth && ak.group_scopes.is_empty() {
            return Err(ConfigError::Validation(
                "auth.authentik.group_scopes must map at least one group when forward_auth is enabled"
                    .into(),
            ));
        }
        if ak.forward_auth
            && ak.trusted_proxies.is_empty()
            && ak.forward_auth_secret.trim().is_empty()
        {
            return Err(ConfigError::Validation(
                "auth.authentik.forward_auth requires trusted_proxies and/or forward_auth_secret \
                 (client-supplied X-authentik-* must not be trusted from any peer)"
                    .into(),
            ));
        }
        for cidr in &ak.trusted_proxies {
            if cidr.parse::<ipnet::IpNet>().is_err() {
                return Err(ConfigError::Validation(format!(
                    "auth.authentik.trusted_proxies: invalid CIDR `{cidr}`"
                )));
            }
        }
        Ok(())
    }

    fn validate_admin_auth(&self) -> Result<(), ConfigError> {
        match self.auth.admin.mode {
            AdminAuthMode::None => Ok(()),
            AdminAuthMode::Basic => {
                // Basic admin needs a valid master hash (even under Authentik MCP).
                // Local provider already validated the same hash — call is idempotent.
                self.validate_master_password_hash()
            }
            AdminAuthMode::Authentik => {
                if self.auth.admin.required_groups.is_empty() {
                    return Err(ConfigError::Validation(
                        "auth.admin.required_groups must list at least one group when \
                         auth.admin.mode = authentik"
                            .into(),
                    ));
                }
                // Same hop trust as MCP forward-auth (settings live under [auth.authentik]).
                let ak = &self.auth.authentik;
                if ak.trusted_proxies.is_empty() && ak.forward_auth_secret.trim().is_empty() {
                    return Err(ConfigError::Validation(
                        "auth.admin.mode = authentik requires auth.authentik.trusted_proxies \
                         and/or forward_auth_secret"
                            .into(),
                    ));
                }
                for cidr in &ak.trusted_proxies {
                    if cidr.parse::<ipnet::IpNet>().is_err() {
                        return Err(ConfigError::Validation(format!(
                            "auth.authentik.trusted_proxies: invalid CIDR `{cidr}`"
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    fn validate_common(&self) -> Result<(), ConfigError> {
        if self.notif_ring_max == 0 {
            return Err(ConfigError::Validation("notif_ring_max must be > 0".into()));
        }
        if !self.mcp_path.starts_with('/') {
            return Err(ConfigError::Validation(
                "mcp_path must start with '/'".into(),
            ));
        }
        if self.proxy.enabled {
            if !self.proxy.mcp_path.starts_with('/') {
                return Err(ConfigError::Validation(
                    "proxy.mcp_path must start with '/'".into(),
                ));
            }
            if self.proxy.mcp_path == self.mcp_path {
                return Err(ConfigError::Validation(format!(
                    "proxy.mcp_path ({}) must differ from mcp_path ({})",
                    self.proxy.mcp_path, self.mcp_path
                )));
            }
        }
        if self.tasks.enabled {
            if self.tasks.max_concurrent == 0 {
                return Err(ConfigError::Validation(
                    "tasks.max_concurrent must be > 0".into(),
                ));
            }
            if self.tasks.db_path.as_os_str().is_empty() {
                return Err(ConfigError::Validation(
                    "tasks.db_path must be set when tasks.enabled = true".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config load: {0}")]
    Load(Box<figment::Error>),
    #[error("config validation: {0}")]
    Validation(String),
}

/// Extract settings from `path` (TOML) + env-var overrides without validating.
fn load_raw(path: Option<&Path>) -> Result<Settings, ConfigError> {
    let default = PathBuf::from("vmcp.toml");
    let toml_path = path.unwrap_or(&default);
    let figment = Figment::new()
        .merge(Toml::file(toml_path))
        .merge(Env::prefixed("VMCP_").split("__"));
    figment
        .extract()
        .map_err(|e| ConfigError::Load(Box::new(e)))
}

/// Load settings from `path` (TOML) + env-var overrides (`VMCP_*`) for HTTP ingress.
///
/// `path` defaults to `./vmcp.toml`. The function does NOT panic if the file
/// is absent — env vars alone can satisfy the schema, useful for containers.
pub fn load(path: Option<&Path>) -> Result<Settings, ConfigError> {
    let settings = load_raw(path)?;
    settings.validate()?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> tempfile_workaround::Tmp {
        let path = std::env::temp_dir().join(format!("vmcp-cfg-test-{}.toml", uuid_like()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        tempfile_workaround::Tmp(path)
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }

    // Tiny RAII wrapper to clean tmp files after each test without pulling in `tempfile`.
    mod tempfile_workaround {
        use std::path::PathBuf;
        pub struct Tmp(pub PathBuf);
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    #[test]
    fn loads_defaults_from_minimal_config() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert_eq!(s.port, 8765);
        assert_eq!(s.mcp_path, "/mcp");
        assert_eq!(s.gql.max_depth, 10);
        assert_eq!(s.notif_ring_max, 10_000);
        assert_eq!(s.auth.clients_db_path, PathBuf::from("state/clients.db"));
        assert_eq!(s.recorder.idle_ttl_secs, 300);
    }

    #[test]
    fn rejects_short_rotation_window() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 3600
token_ttl_secs = 3600
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err();
        let s = format!("{}", err);
        assert!(s.contains("jwks_rotate_secs"), "got: {}", s);
    }

    #[test]
    fn rejects_invalid_argon2() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "plaintext"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err();
        let s = format!("{}", err);
        assert!(s.contains("argon2"), "got: {}", s);
    }

    #[test]
    fn gql_cap_defaults_to_error_with_1mib() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        // Defaults kick in when neither field is set in TOML.
        assert_eq!(s.gql.max_response_bytes, 1_048_576);
        assert_eq!(s.gql.response_cap_mode, CapMode::Error);
    }

    #[test]
    fn proxy_disabled_by_default() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert!(!s.proxy.enabled);
        assert_eq!(s.proxy.mcp_path, "/mcp-proxy");
    }

    #[test]
    fn proxy_path_collision_rejected() {
        let tmp = write_tmp(
            r#"
[proxy]
enabled = true
mcp_path = "/mcp"

[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err();
        let s = format!("{}", err);
        assert!(s.contains("proxy.mcp_path"), "got: {}", s);
    }

    #[test]
    fn proxy_enabled_with_custom_path() {
        let tmp = write_tmp(
            r#"
[proxy]
enabled = true
mcp_path = "/mcp-raw"

[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert!(s.proxy.enabled);
        assert_eq!(s.proxy.mcp_path, "/mcp-raw");
    }

    #[test]
    fn auth_enabled_by_default() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert!(s.auth.enabled, "auth.enabled defaults to true");
    }

    #[test]
    fn placeholder_master_hash_is_rejected() {
        // The `…$REPLACE_ME` placeholder shipped in vmcp.toml starts with
        // `$argon2` but its trailing segment is not valid Base64. It must be
        // rejected at load time, not silently accepted (which previously only
        // failed at the OAuth /consent step).
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$REPLACE_ME"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let err = load(Some(&tmp.0)).expect_err("placeholder hash must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("not a valid argon2 hash"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("vmcp hash-password"),
            "message should be actionable: {msg}"
        );
    }

    #[test]
    fn valid_master_hash_passes() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        load(Some(&tmp.0)).expect("valid PHC hash should load");
    }

    #[test]
    fn auth_disabled_skips_hash_validation() {
        let tmp = write_tmp(
            r#"
[auth]
enabled = false
master_password_argon2 = ""
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads with empty hash when auth disabled");
        assert!(!s.auth.enabled);
        assert!(s.auth.master_password_argon2.is_empty());
    }

    #[test]
    fn auth_disabled_skips_rotation_validation() {
        let tmp = write_tmp(
            r#"
[auth]
enabled = false
master_password_argon2 = ""
jwt_kid = "k1"
jwks_rotate_secs = 100
token_ttl_secs = 9999
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads with short rotation when auth disabled");
        assert!(!s.auth.enabled);
    }

    #[test]
    fn auth_disabled_block_omitting_oauth_fields_parses() {
        // Mirrors a minimal operator config: `[auth] enabled = false` with only
        // `tokens_file`, omitting master_password_argon2 / jwt_kid / rotation /
        // ttl. These must default rather than fail deserialization.
        let tmp = write_tmp(
            r#"
public_base_url = "http://localhost:4000"

[auth]
enabled = false
tokens_file = "./tokens.json"
"#,
        );
        let s = load(Some(&tmp.0)).expect("minimal disabled auth block parses");
        assert!(!s.auth.enabled);
        assert!(s.auth.master_password_argon2.is_empty());
        assert_eq!(s.auth.jwt_kid, "vmcp");
        assert_eq!(s.auth.jwks_rotate_secs, 86_400);
        assert_eq!(s.auth.token_ttl_secs, 3_600);
        assert_eq!(
            s.auth.tokens_file.as_deref(),
            Some(std::path::Path::new("./tokens.json"))
        );
        assert_eq!(
            s.auth.clients_db_path,
            PathBuf::from("state/clients.db"),
            "clients_db_path defaults even when omitted from a minimal [auth] block"
        );
    }

    #[test]
    fn auth_section_omitted_entirely_defaults_but_fails_validation() {
        // Omitting `[auth]` leaves auth.enabled=true with an empty argon2 hash,
        // which HTTP ingress validation must reject.
        let tmp = write_tmp(
            r#"
public_base_url = "http://localhost:4000"
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err().to_string();
        assert!(
            err.contains("master_password_argon2"),
            "expected argon2 validation error, got: {err}"
        );
    }

    #[test]
    fn auth_enabled_empty_hash_rejected() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = ""
jwt_kid = "unused"
jwks_rotate_secs = 100
token_ttl_secs = 9999

[gql]
max_depth = 10
max_complexity = 1000

[upstream]
spawn_timeout_ms = 30000
call_timeout_ms = 60000
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err().to_string();
        assert!(
            err.contains("master_password_argon2") || err.contains("jwks_rotate"),
            "expected auth validation error, got: {err}"
        );
    }

    #[test]
    fn tasks_disabled_by_default() {
        let tmp = write_tmp(
            r#"
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert!(!s.tasks.enabled);
        assert_eq!(s.tasks.db_path, PathBuf::from("state/tasks.db"));
        assert_eq!(s.tasks.max_concurrent, 16);
    }

    #[test]
    fn tasks_enabled_reads_overrides() {
        let tmp = write_tmp(
            r#"
[tasks]
enabled = true
db_path = "/tmp/vmcp-tasks.db"
task_ttl_ms = 60000
poll_interval_ms = 1500
max_concurrent = 4

[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert!(s.tasks.enabled);
        assert_eq!(s.tasks.db_path, PathBuf::from("/tmp/vmcp-tasks.db"));
        assert_eq!(s.tasks.task_ttl_ms, 60_000);
        assert_eq!(s.tasks.poll_interval_ms, 1_500);
        assert_eq!(s.tasks.max_concurrent, 4);
    }

    #[test]
    fn tasks_enabled_rejects_zero_concurrency() {
        let tmp = write_tmp(
            r#"
[tasks]
enabled = true
max_concurrent = 0

[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err();
        assert!(
            format!("{err}").contains("tasks.max_concurrent"),
            "got: {err}"
        );
    }

    #[test]
    fn gql_cap_reads_overrides_from_toml() {
        let tmp = write_tmp(
            r#"
[gql]
max_depth = 10
max_complexity = 1000
max_response_bytes = 2097152
response_cap_mode = "truncate"

[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$dG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4tdG9rZW4"
jwt_kid = "k1"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        );
        let s = load(Some(&tmp.0)).expect("loads");
        assert_eq!(s.gql.max_response_bytes, 2_097_152);
        assert_eq!(s.gql.response_cap_mode, CapMode::Truncate);
    }

    #[test]
    fn authentik_provider_parses_without_master_password() {
        let tmp = write_tmp(
            r#"
[auth]
enabled = true
provider = "authentik"

[auth.admin]
mode = "authentik"
required_groups = ["mcp-admins"]

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
audiences = ["https://mcp.example.com/mcp"]
forward_auth = true
accept_bearer = true
trusted_proxies = ["10.244.0.0/16"]
group_scopes = { "mcp-users" = "mcp:use" }
"#,
        );
        let s = load(Some(&tmp.0)).expect("authentik auth loads");
        assert_eq!(s.auth.provider, AuthProviderKind::Authentik);
        assert_eq!(s.auth.admin.mode, AdminAuthMode::Authentik);
        assert!(s.auth.master_password_argon2.is_empty());
        assert_eq!(
            s.auth.authentik.issuer,
            "https://auth.example.com/application/o/mcp-internal/"
        );
        assert_eq!(
            s.auth
                .authentik
                .group_scopes
                .get("mcp-users")
                .map(String::as_str),
            Some("mcp:use")
        );
        assert_eq!(s.auth.authentik.trusted_proxies, vec!["10.244.0.0/16"]);
    }

    #[test]
    fn authentik_requires_group_scopes_when_forward_auth() {
        let tmp = write_tmp(
            r#"
[auth]
provider = "authentik"

[auth.admin]
mode = "none"

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
forward_auth = true
trusted_proxies = ["10.0.0.0/8"]
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err().to_string();
        assert!(
            err.contains("group_scopes"),
            "expected group_scopes validation, got: {err}"
        );
    }

    #[test]
    fn authentik_requires_trust_when_forward_auth() {
        let tmp = write_tmp(
            r#"
[auth]
provider = "authentik"

[auth.admin]
mode = "none"

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
forward_auth = true
group_scopes = { "mcp-users" = "mcp:use" }
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err().to_string();
        assert!(
            err.contains("trusted_proxies") || err.contains("forward_auth_secret"),
            "expected hop trust validation, got: {err}"
        );
    }

    #[test]
    fn admin_basic_requires_master_even_under_authentik_mcp() {
        let tmp = write_tmp(
            r#"
[auth]
provider = "authentik"
# admin.mode defaults to basic → needs master hash

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
forward_auth_secret = "gateway-hop"
group_scopes = { "mcp-users" = "mcp:use" }
"#,
        );
        let err = load(Some(&tmp.0)).unwrap_err().to_string();
        assert!(
            err.contains("master_password_argon2"),
            "expected master hash required for admin.basic, got: {err}"
        );
    }

    #[test]
    fn admin_none_allows_authentik_mcp_without_master() {
        let tmp = write_tmp(
            r#"
[auth]
provider = "authentik"

[auth.admin]
mode = "none"

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
forward_auth_secret = "gateway-hop"
group_scopes = { "mcp-users" = "mcp:use" }
"#,
        );
        let s = load(Some(&tmp.0)).expect("admin.none + authentik mcp");
        assert_eq!(s.auth.admin.mode, AdminAuthMode::None);
    }
}
