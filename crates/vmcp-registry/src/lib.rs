//! Upstream registry + tools lock.
//!
//! Replaces the Python `vmcp/registry/{loader,models,resolver}.py` chain,
//! simplified to just hand-written `registry.json` (no npm/oci/mcpb fetching
//! — that's a v1.1 concern).
//!
//! Files:
//! - `registry.json` — human-edited, lists upstreams to spawn
//! - `tools.lock.json` — generated, the snapshot of upstream tools/list,
//!   used for drift detection and to determine readOnlyHint defaults for
//!   the GraphQL Query/Mutation bucketing

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Registry of upstreams to spawn at boot.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Upstream MCP servers to spawn / connect. Key must be `upstreams`
    /// (legacy `servers` is a hard error as of 1.0).
    #[serde(default)]
    pub upstreams: Vec<UpstreamSpec>,
}

/// Transport used to reach an upstream MCP server.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamTransport {
    /// Spawn a local child process and speak MCP over its stdio pipes.
    #[default]
    Stdio,
    /// Connect to a remote MCP server over Streamable HTTP.
    Http,
}

/// One upstream MCP server entry. Either a spawned stdio child process
/// (`transport = "stdio"`, the default) or a remote Streamable-HTTP server
/// (`transport = "http"`, requires `url`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamSpec {
    /// Logical name. Becomes the GraphQL namespace (PascalCase) and the
    /// `source` of forwarded notifications. Must be a valid identifier.
    pub name: String,
    /// Operator-authored description shown via `Query.servers.description`.
    /// Lets agents pick the right upstream by purpose before reaching for
    /// `search(q)` — e.g. "WORK for JIRA — read & write issues".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How to reach this upstream. Defaults to `stdio`.
    #[serde(default, skip_serializing_if = "UpstreamTransport::is_stdio")]
    pub transport: UpstreamTransport,
    /// Streamable-HTTP endpoint URL (required when `transport = "http"`),
    /// e.g. `http://127.0.0.1:8080/mcp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Bearer token sent as `Authorization: Bearer <token>` to an HTTP
    /// upstream. The raw token only — vmcp/rmcp adds the `Bearer ` prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
    /// Executable to spawn (stdio transport). Ignored for HTTP upstreams.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the child process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory for the child process. None = inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Path to a sidecar JSON with `readOnlyHint` overrides for tools whose
    /// upstream annotation is missing or wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_spec: Option<PathBuf>,
    /// Whether to spawn this upstream. Disabled entries are skipped.
    /// Serde's `bool` default is `false`, so we need an explicit default fn.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Forward authenticated caller identity (`X-Vmcp-Subject` / `X-Vmcp-Groups`)
    /// on HTTP upstream tool calls. Default true. Registry `bearer` remains the
    /// service credential on `Authorization`.
    #[serde(default = "default_true")]
    pub forward_identity: bool,
}

fn default_true() -> bool {
    true
}

impl UpstreamTransport {
    fn is_stdio(&self) -> bool {
        matches!(self, Self::Stdio)
    }
}

impl UpstreamSpec {
    /// Build a stdio upstream (`command` + `args`).
    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            transport: UpstreamTransport::Stdio,
            url: None,
            bearer: None,
            command: command.into(),
            args,
            env: BTreeMap::new(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
            forward_identity: true,
        }
    }

    /// Build an HTTP upstream (`url`, optional bearer).
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            transport: UpstreamTransport::Http,
            url: Some(url.into()),
            bearer: None,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
            forward_identity: true,
        }
    }
}

/// Optional per-tool sidecar override file: `specs/<server>.json`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SidecarSpec {
    pub server: String,
    #[serde(default)]
    pub tools: Vec<SidecarTool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SidecarTool {
    pub name: String,
    /// Override for the upstream's `readOnlyHint` annotation.
    #[serde(default)]
    pub read_only: bool,
    /// Optional override description (rarely needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Override for upstream `execution.taskSupport`. When set, controls whether
    /// the tool appears on vmcp's `run_task` allowlist (`optional`/`required`).
    /// `forbidden` (or omitting with upstream also forbidden) keeps it GraphQL-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_support: Option<TaskSupportHint>,
}

/// SEP-1686 `execution.taskSupport` as stored in the tools lock / sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskSupportHint {
    /// Not a task tool (default).
    #[default]
    Forbidden,
    /// May be invoked via `run_task` with or without `task`.
    Optional,
    /// Must be invoked as a task when using `run_task`.
    Required,
}

impl TaskSupportHint {
    pub fn is_task(self) -> bool {
        matches!(self, Self::Optional | Self::Required)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

/// Snapshot of upstream tools used for drift detection.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolsLock {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub entries: Vec<LockEntry>,
}

impl ToolsLock {
    pub fn new(entries: Vec<LockEntry>) -> Self {
        Self {
            version: 1,
            generated_at: Utc::now(),
            entries,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockEntry {
    /// Upstream name (matches `UpstreamSpec.name`).
    pub server: String,
    /// Snapshot of tools/list as observed at lock time.
    pub tools: Vec<CachedTool>,
    /// Sidecar overrides applied at lock time (audit trail).
    #[serde(default)]
    pub resolved_overrides: Vec<SidecarTool>,
}

/// Tool snapshot — just enough to detect shape changes and route resolvers.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CachedTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema of the tool's input (passed verbatim to GraphQL builder).
    pub input_schema: serde_json::Value,
    /// readOnlyHint (sidecar-merged). True → Query bucket; false → Mutation.
    #[serde(default)]
    pub read_only: bool,
    /// `execution.taskSupport` (sidecar-merged). Non-forbidden → `run_task` allowlist.
    #[serde(default)]
    pub task_support: TaskSupportHint,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("env: {0}")]
    Env(String),
    #[error("duplicate upstream name: {0}")]
    DuplicateName(String),
    #[error("unknown upstream name: {0}")]
    UnknownName(String),
    #[error("invalid upstream name: {0}")]
    InvalidName(String),
}

/// Validate an upstream `name`: non-empty, ≤64 chars, `[A-Za-z0-9_-]+`.
/// Same charset as `upstream:<name>` scope tokens.
pub fn validate_upstream_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty() || name.len() > 64 {
        return Err(RegistryError::InvalidName(
            "must be 1..=64 characters".into(),
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(RegistryError::InvalidName(
            "use only A-Za-z0-9_- (GraphQL / scope identifier)".into(),
        ));
    }
    Ok(())
}

/// Expand `${VAR}` / `$VAR` placeholders from the process environment.
///
/// Missing variables are an **error** (strict) so a typo cannot silently
/// produce `https://host//mcp` or drop a bearer. Used for HTTP upstream
/// `url` / `bearer` / `env`.
pub fn expand_env(input: &str) -> Result<String, RegistryError> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let key = &input[i + 2..i + 2 + end];
                if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    let val = std::env::var(key).map_err(|_| {
                        RegistryError::Env(format!("environment variable `{key}` is not set"))
                    })?;
                    out.push_str(&val);
                    i += 3 + end;
                    continue;
                }
            }
            out.push('$');
            i += 1;
            continue;
        }
        // $VAR
        let rest = &input[i + 1..];
        let len = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .map(|c| c.len_utf8())
            .sum::<usize>();
        if len > 0 {
            let key = &rest[..len];
            let val = std::env::var(key).map_err(|_| {
                RegistryError::Env(format!("environment variable `{key}` is not set"))
            })?;
            out.push_str(&val);
            i += 1 + len;
        } else {
            out.push('$');
            i += 1;
        }
    }
    Ok(out)
}

/// Expand `${ENV}` in `url` / `bearer` / `env` on a clone (CLI probe path).
pub fn expand_upstream_spec(spec: &UpstreamSpec) -> Result<UpstreamSpec, RegistryError> {
    let mut out = spec.clone();
    expand_upstream(&mut out)?;
    Ok(out)
}

fn expand_upstream(spec: &mut UpstreamSpec) -> Result<(), RegistryError> {
    if let Some(url) = spec.url.as_mut() {
        *url = expand_env(url)?;
    }
    if let Some(bearer) = spec.bearer.as_mut() {
        let expanded = expand_env(bearer)?;
        if expanded.is_empty() {
            tracing::warn!(
                upstream = %spec.name,
                "bearer expanded to empty; dropping Authorization header"
            );
            spec.bearer = None;
        } else {
            *bearer = expanded;
        }
    }
    for v in spec.env.values_mut() {
        *v = expand_env(v)?;
    }
    Ok(())
}

/// Soft cap against accidental huge registries (G33).
pub const MAX_UPSTREAMS: usize = 256;

fn reject_duplicate_names(registry: &Registry) -> Result<(), RegistryError> {
    if registry.upstreams.len() > MAX_UPSTREAMS {
        return Err(RegistryError::Env(format!(
            "too many upstreams ({}); max is {MAX_UPSTREAMS}",
            registry.upstreams.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for u in &registry.upstreams {
        if !seen.insert(u.name.as_str()) {
            return Err(RegistryError::DuplicateName(u.name.clone()));
        }
    }
    Ok(())
}

/// Load registry JSON from disk **without** expanding `${ENV}` placeholders.
/// Use this for CLI edit/list so secrets stay as `${VAR}` on round-trip.
///
/// Returns an empty registry if the file is absent. Duplicate `name`s are
/// rejected.
pub fn load_registry_raw(path: &Path) -> Result<Registry, RegistryError> {
    if !path.exists() {
        tracing::warn!(
            ?path,
            "registry file not found, starting with empty upstream list"
        );
        return Ok(Registry::default());
    }
    let text = fs::read_to_string(path)?;
    let registry: Registry = serde_json::from_str(&text)?;
    reject_duplicate_names(&registry)?;
    Ok(registry)
}

/// Load the registry JSON. Returns an empty registry if the file is absent —
/// vmcp can boot with no upstreams (useful for OAuth-only deploys).
///
/// After parse, expands `${ENV}` placeholders in each upstream's `url`,
/// `bearer`, and `env` values. Duplicate `name`s are rejected. Missing env
/// vars in placeholders are a hard error.
pub fn load_registry(path: &Path) -> Result<Registry, RegistryError> {
    let mut registry = load_registry_raw(path)?;
    for upstream in &mut registry.upstreams {
        expand_upstream(upstream)?;
    }
    Ok(registry)
}

/// Atomic write of `registry.json`: `<path>.tmp` → backup → rename.
/// Does **not** expand env — writes the in-memory specs as-is (CLI round-trip).
pub fn save_registry_atomic(path: &Path, registry: &Registry) -> Result<(), RegistryError> {
    reject_duplicate_names(registry)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    let bak = path.with_extension("json.bak");
    let text = serde_json::to_string_pretty(registry)?;
    fs::write(&tmp, format!("{text}\n"))?;
    if path.exists() {
        let _ = fs::rename(path, &bak);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Append an upstream. Errors on invalid / duplicate name or cap overflow.
pub fn add_upstream(registry: &mut Registry, spec: UpstreamSpec) -> Result<(), RegistryError> {
    validate_upstream_name(&spec.name)?;
    if registry.upstreams.iter().any(|u| u.name == spec.name) {
        return Err(RegistryError::DuplicateName(spec.name));
    }
    if registry.upstreams.len() >= MAX_UPSTREAMS {
        return Err(RegistryError::Env(format!(
            "too many upstreams ({}); max is {MAX_UPSTREAMS}",
            registry.upstreams.len()
        )));
    }
    match spec.transport {
        UpstreamTransport::Http => {
            if spec.url.as_deref().unwrap_or("").is_empty() {
                return Err(RegistryError::Env(
                    "http upstream requires a non-empty url".into(),
                ));
            }
        }
        UpstreamTransport::Stdio => {
            if spec.command.is_empty() {
                return Err(RegistryError::Env(
                    "stdio upstream requires a command".into(),
                ));
            }
        }
    }
    registry.upstreams.push(spec);
    Ok(())
}

/// Remove an upstream by name. Errors if missing.
pub fn remove_upstream(registry: &mut Registry, name: &str) -> Result<UpstreamSpec, RegistryError> {
    let idx = registry
        .upstreams
        .iter()
        .position(|u| u.name == name)
        .ok_or_else(|| RegistryError::UnknownName(name.to_string()))?;
    Ok(registry.upstreams.remove(idx))
}

/// Look up an upstream by name.
pub fn get_upstream<'a>(registry: &'a Registry, name: &str) -> Option<&'a UpstreamSpec> {
    registry.upstreams.iter().find(|u| u.name == name)
}

/// Load the lock file. Returns None if absent (first boot).
pub fn load_lock(path: &Path) -> Result<Option<ToolsLock>, RegistryError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let lock: ToolsLock = serde_json::from_str(&text)?;
    Ok(Some(lock))
}

/// Atomic write: `<path>.tmp` → backup current to `<path>.bak` → rename tmp.
/// Mirror of Python `_atomic_write_json` in `demo_project/vmcp/registry/loader.py`.
pub fn save_lock_atomic(path: &Path, lock: &ToolsLock) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bak = path.with_extension("json.bak");
    let text = serde_json::to_string_pretty(lock)?;
    fs::write(&tmp, &text)?;
    if path.exists() {
        // Best-effort backup. If it fails we still want the new lock to land.
        let _ = fs::rename(path, &bak);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a sidecar spec by path. Returns Ok(None) if path is None or file
/// absent — sidecars are strictly optional.
pub fn load_sidecar(path: Option<&Path>) -> Result<Option<SidecarSpec>, RegistryError> {
    match path {
        Some(p) if p.exists() => {
            let text = fs::read_to_string(p)?;
            let spec: SidecarSpec = serde_json::from_str(&text)?;
            Ok(Some(spec))
        }
        _ => Ok(None),
    }
}

/// Atomic write of a sidecar JSON (`specs/<server>.json`).
pub fn save_sidecar_atomic(path: &Path, spec: &SidecarSpec) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    let bak = path.with_extension("json.bak");
    let text = serde_json::to_string_pretty(spec)?;
    fs::write(&tmp, format!("{text}\n"))?;
    if path.exists() {
        let _ = fs::rename(path, &bak);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Insert or replace a tool override in a sidecar (by tool name).
pub fn upsert_sidecar_tool(spec: &mut SidecarSpec, tool: SidecarTool) {
    if let Some(existing) = spec.tools.iter_mut().find(|t| t.name == tool.name) {
        *existing = tool;
    } else {
        spec.tools.push(tool);
    }
    spec.tools.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Remove a tool override by name. Returns the removed entry.
pub fn remove_sidecar_tool(
    spec: &mut SidecarSpec,
    tool_name: &str,
) -> Result<SidecarTool, RegistryError> {
    let idx = spec
        .tools
        .iter()
        .position(|t| t.name == tool_name)
        .ok_or_else(|| RegistryError::UnknownName(format!("tool `{tool_name}`")))?;
    Ok(spec.tools.remove(idx))
}

/// Default on-disk name for a server's sidecar under `spec_dir`.
pub fn sidecar_filename(server: &str) -> String {
    format!("{server}.json")
}

/// Build a sidecar from a live `tools/list` snapshot (CLI `add mcp` codegen).
///
/// `task_support = forbidden` is omitted (sidecar default); descriptions are
/// kept when present so operators can skim the file.
pub fn sidecar_from_cached_tools(server: &str, tools: &[CachedTool]) -> SidecarSpec {
    let mut tools: Vec<SidecarTool> = tools
        .iter()
        .map(|t| SidecarTool {
            name: t.name.clone(),
            read_only: t.read_only,
            description: t.description.clone(),
            task_support: match t.task_support {
                TaskSupportHint::Forbidden => None,
                other => Some(other),
            },
        })
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    SidecarSpec {
        server: server.to_string(),
        tools,
    }
}

/// Shape-only drift check: do the tool names + JSON schemas + read_only +
/// task_support match? Description differences are ignored (descriptions
/// change frequently and don't break the schema).
pub fn detect_drift(stored: &[CachedTool], live: &[CachedTool]) -> bool {
    if stored.len() != live.len() {
        return true;
    }
    let mut stored_by_name: BTreeMap<&str, &CachedTool> =
        stored.iter().map(|t| (t.name.as_str(), t)).collect();
    for tool in live {
        match stored_by_name.remove(tool.name.as_str()) {
            None => return true,
            Some(s) => {
                if s.input_schema != tool.input_schema
                    || s.read_only != tool.read_only
                    || s.task_support != tool.task_support
                {
                    return true;
                }
            }
        }
    }
    !stored_by_name.is_empty()
}

/// Apply sidecar overrides on top of a list of CachedTool produced by the
/// upstream. Returns (tools_with_overrides_applied, audit_trail).
pub fn apply_sidecar(
    tools: Vec<CachedTool>,
    sidecar: Option<&SidecarSpec>,
) -> (Vec<CachedTool>, Vec<SidecarTool>) {
    let Some(sc) = sidecar else {
        return (tools, Vec::new());
    };
    let by_name: BTreeMap<&str, &SidecarTool> =
        sc.tools.iter().map(|t| (t.name.as_str(), t)).collect();
    let audit: Vec<SidecarTool> = sc.tools.clone();
    let merged = tools
        .into_iter()
        .map(|mut t| {
            if let Some(ov) = by_name.get(t.name.as_str()) {
                t.read_only = ov.read_only;
                if let Some(desc) = &ov.description {
                    t.description = Some(desc.clone());
                }
                if let Some(ts) = ov.task_support {
                    t.task_support = ts;
                }
            }
            t
        })
        .collect();
    (merged, audit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_path(stem: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{}-{}.json", stem, nanos))
    }

    fn cleanup(paths: &[PathBuf]) {
        for p in paths {
            let _ = fs::remove_file(p);
            let _ = fs::remove_file(p.with_extension("json.bak"));
            let _ = fs::remove_file(p.with_extension("json.tmp"));
        }
    }

    #[test]
    fn registry_requires_upstreams_key_rejects_servers() {
        let with_upstreams = r#"{"upstreams": [{"name": "x", "command": "echo"}]}"#;
        let r: Registry = serde_json::from_str(with_upstreams).unwrap();
        assert_eq!(r.upstreams.len(), 1);
        assert_eq!(r.upstreams[0].name, "x");

        let with_servers = r#"{"servers": [{"name": "x", "command": "echo"}]}"#;
        let err = serde_json::from_str::<Registry>(with_servers).unwrap_err();
        assert!(
            err.to_string().contains("servers") || err.to_string().contains("unknown field"),
            "legacy servers key must fail parse, got: {err}"
        );
    }

    #[test]
    fn upstream_transport_defaults_to_stdio() {
        let r: Registry =
            serde_json::from_str(r#"{"upstreams": [{"name": "x", "command": "echo"}]}"#).unwrap();
        assert_eq!(r.upstreams[0].transport, UpstreamTransport::Stdio);
        assert!(r.upstreams[0].url.is_none());
    }

    #[test]
    fn upstream_http_transport_parses_url_and_bearer() {
        let r: Registry = serde_json::from_str(
            r#"{"upstreams": [{
                "name": "dagu",
                "transport": "http",
                "url": "http://127.0.0.1:8080/mcp",
                "bearer": "dagu_abc"
            }]}"#,
        )
        .unwrap();
        let u = &r.upstreams[0];
        assert_eq!(u.transport, UpstreamTransport::Http);
        assert_eq!(u.url.as_deref(), Some("http://127.0.0.1:8080/mcp"));
        assert_eq!(u.bearer.as_deref(), Some("dagu_abc"));
        // command is optional for http upstreams.
        assert!(u.command.is_empty());
    }

    #[test]
    fn expand_env_substitutes_braced_and_bare() {
        std::env::set_var("VMCP_TEST_EXPAND_A", "alpha");
        std::env::set_var("VMCP_TEST_EXPAND_B", "beta");
        assert_eq!(
            expand_env("https://ex/${VMCP_TEST_EXPAND_A}/mcp").unwrap(),
            "https://ex/alpha/mcp"
        );
        assert_eq!(
            expand_env("tok-$VMCP_TEST_EXPAND_B-end").unwrap(),
            "tok-beta-end"
        );
        let err = expand_env("no/${VMCP_TEST_EXPAND_MISSING}/x").unwrap_err();
        assert!(
            err.to_string().contains("VMCP_TEST_EXPAND_MISSING"),
            "missing env must hard-fail, got: {err}"
        );
        std::env::remove_var("VMCP_TEST_EXPAND_A");
        std::env::remove_var("VMCP_TEST_EXPAND_B");
    }

    #[test]
    fn load_registry_rejects_duplicate_names() {
        let p = tmp_path("reg-dup");
        fs::write(
            &p,
            r#"{"upstreams":[
              {"name":"a","transport":"http","url":"http://127.0.0.1:1/mcp"},
              {"name":"a","transport":"http","url":"http://127.0.0.1:2/mcp"}
            ]}"#,
        )
        .unwrap();
        let err = load_registry(&p).unwrap_err();
        assert!(
            matches!(err, RegistryError::DuplicateName(ref n) if n == "a"),
            "got: {err}"
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_registry_expands_bearer_and_drops_empty() {
        let p = tmp_path("reg-expand");
        std::env::set_var("VMCP_TEST_BEARER", "secret-tok");
        std::env::set_var("VMCP_TEST_EMPTY_BEARER", "");
        fs::write(
            &p,
            r#"{"upstreams":[
              {"name":"a","transport":"http","url":"https://ex/${VMCP_TEST_BEARER}/mcp","bearer":"${VMCP_TEST_BEARER}"},
              {"name":"b","transport":"http","url":"https://ex/mcp","bearer":"${VMCP_TEST_EMPTY_BEARER}"}
            ]}"#,
        )
        .unwrap();
        let r = load_registry(&p).unwrap();
        assert_eq!(
            r.upstreams[0].url.as_deref(),
            Some("https://ex/secret-tok/mcp")
        );
        assert_eq!(r.upstreams[0].bearer.as_deref(), Some("secret-tok"));
        assert_eq!(r.upstreams[1].bearer, None);
        std::env::remove_var("VMCP_TEST_BEARER");
        std::env::remove_var("VMCP_TEST_EMPTY_BEARER");
        cleanup(&[p]);
    }

    #[test]
    fn detect_drift_picks_up_tool_addition() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        let live = vec![
            CachedTool {
                name: "a".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                read_only: true,
                task_support: TaskSupportHint::Forbidden,
            },
            CachedTool {
                name: "b".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                read_only: false,
                task_support: TaskSupportHint::Forbidden,
            },
        ];
        assert!(detect_drift(&stored, &live));
    }

    #[test]
    fn detect_drift_picks_up_schema_change() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        assert!(detect_drift(&stored, &live));
    }

    #[test]
    fn detect_drift_ignores_description() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: Some("old".into()),
            input_schema: json!({}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: Some("new".into()),
            input_schema: json!({}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        assert!(!detect_drift(&stored, &live));
    }

    #[test]
    fn detect_drift_picks_up_readonly_flip() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({}),
            read_only: true,
            task_support: TaskSupportHint::Forbidden,
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({}),
            read_only: false,
            task_support: TaskSupportHint::Forbidden,
        }];
        assert!(detect_drift(&stored, &live));
    }

    #[test]
    fn save_lock_atomic_creates_backup_on_overwrite() {
        let path = tmp_path("vmcp-lock");
        let lock1 = ToolsLock::new(vec![]);
        save_lock_atomic(&path, &lock1).unwrap();
        let lock2 = ToolsLock::new(vec![LockEntry {
            server: "x".into(),
            tools: vec![],
            resolved_overrides: vec![],
        }]);
        save_lock_atomic(&path, &lock2).unwrap();

        assert!(path.exists());
        assert!(path.with_extension("json.bak").exists());

        let loaded = load_lock(&path).unwrap().unwrap();
        assert_eq!(loaded.entries.len(), 1);

        cleanup(&[path]);
    }

    #[test]
    fn apply_sidecar_overrides_readonly() {
        let tools = vec![CachedTool {
            name: "danger".into(),
            description: None,
            input_schema: json!({}),
            read_only: true, // upstream said read-only…
            task_support: TaskSupportHint::Forbidden,
        }];
        let sc = SidecarSpec {
            server: "x".into(),
            tools: vec![SidecarTool {
                name: "danger".into(),
                read_only: false, // …operator overrides it.
                description: None,
                task_support: None,
            }],
        };
        let (out, _audit) = apply_sidecar(tools, Some(&sc));
        assert!(!out[0].read_only);
    }

    #[test]
    fn task_support_hint_helpers() {
        assert!(!TaskSupportHint::Forbidden.is_task());
        assert!(TaskSupportHint::Optional.is_task());
        assert!(TaskSupportHint::Required.is_task());
        assert_eq!(TaskSupportHint::Forbidden.as_str(), "forbidden");
        assert_eq!(TaskSupportHint::Optional.as_str(), "optional");
        assert_eq!(TaskSupportHint::Required.as_str(), "required");
    }

    #[test]
    fn apply_sidecar_overrides_task_support() {
        let tools = vec![CachedTool {
            name: "build".into(),
            description: None,
            input_schema: json!({}),
            read_only: false,
            task_support: TaskSupportHint::Forbidden,
        }];
        let sc = SidecarSpec {
            server: "p".into(),
            tools: vec![SidecarTool {
                name: "build".into(),
                read_only: false,
                description: Some("long".into()),
                task_support: Some(TaskSupportHint::Optional),
            }],
        };
        let (out, _) = apply_sidecar(tools, Some(&sc));
        assert_eq!(out[0].task_support, TaskSupportHint::Optional);
        assert_eq!(out[0].description.as_deref(), Some("long"));
    }

    #[test]
    fn detect_drift_picks_up_task_support_flip() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({}),
            read_only: false,
            task_support: TaskSupportHint::Forbidden,
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({}),
            read_only: false,
            task_support: TaskSupportHint::Optional,
        }];
        assert!(detect_drift(&stored, &live));
    }

    #[test]
    fn validate_upstream_name_charset() {
        assert!(validate_upstream_name("time").is_ok());
        assert!(validate_upstream_name("architect_c4").is_ok());
        assert!(validate_upstream_name("my-server").is_ok());
        assert!(validate_upstream_name("").is_err());
        assert!(validate_upstream_name("has space").is_err());
        assert!(validate_upstream_name("bad.name").is_err());
    }

    #[test]
    fn add_remove_upstream_round_trip_preserves_env_placeholders() {
        let path = tmp_path("reg-cli");
        let mut reg = Registry::default();
        let mut spec = UpstreamSpec::http("ctx", "https://mcp.example.com/${API_KEY}/mcp");
        spec.bearer = Some("${API_KEY}".into());
        add_upstream(&mut reg, spec).unwrap();
        save_registry_atomic(&path, &reg).unwrap();

        let loaded = load_registry_raw(&path).unwrap();
        assert_eq!(
            loaded.upstreams[0].url.as_deref(),
            Some("https://mcp.example.com/${API_KEY}/mcp")
        );
        assert_eq!(loaded.upstreams[0].bearer.as_deref(), Some("${API_KEY}"));

        remove_upstream(&mut reg, "ctx").unwrap();
        assert!(get_upstream(&reg, "ctx").is_none());
        assert!(matches!(
            add_upstream(&mut reg, UpstreamSpec::http("ctx", "http://x")),
            Ok(())
        ));
        assert!(matches!(
            add_upstream(&mut reg, UpstreamSpec::http("ctx", "http://y")),
            Err(RegistryError::DuplicateName(_))
        ));
        cleanup(&[path]);
    }

    #[test]
    fn serialize_skips_stdio_defaults() {
        let spec = UpstreamSpec::stdio("time", "uvx", vec!["mcp-server-time".into()]);
        let v = serde_json::to_value(&spec).unwrap();
        assert!(v.get("transport").is_none());
        assert!(v.get("url").is_none());
        assert_eq!(v["enabled"], true);
        assert_eq!(v["command"], "uvx");
    }

    #[test]
    fn sidecar_from_cached_tools_omits_forbidden_task() {
        let tools = vec![
            CachedTool {
                name: "read".into(),
                description: Some("r".into()),
                input_schema: json!({}),
                read_only: true,
                task_support: TaskSupportHint::Forbidden,
            },
            CachedTool {
                name: "build".into(),
                description: None,
                input_schema: json!({}),
                read_only: false,
                task_support: TaskSupportHint::Optional,
            },
        ];
        let sc = sidecar_from_cached_tools("p", &tools);
        assert_eq!(sc.server, "p");
        assert_eq!(sc.tools.len(), 2);
        assert_eq!(sc.tools[0].name, "build"); // sorted
        assert_eq!(sc.tools[0].task_support, Some(TaskSupportHint::Optional));
        assert_eq!(sc.tools[1].task_support, None);
        assert!(sc.tools[1].read_only);
    }
}
