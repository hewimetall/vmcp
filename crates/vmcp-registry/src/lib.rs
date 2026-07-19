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
    #[serde(default)]
    pub description: Option<String>,
    /// How to reach this upstream. Defaults to `stdio`.
    #[serde(default)]
    pub transport: UpstreamTransport,
    /// Streamable-HTTP endpoint URL (required when `transport = "http"`),
    /// e.g. `http://127.0.0.1:8080/mcp`.
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token sent as `Authorization: Bearer <token>` to an HTTP
    /// upstream. The raw token only — vmcp/rmcp adds the `Bearer ` prefix.
    #[serde(default)]
    pub bearer: Option<String>,
    /// Executable to spawn (stdio transport). Ignored for HTTP upstreams.
    #[serde(default)]
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for the child process. None = inherit.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Path to a sidecar JSON with `readOnlyHint` overrides for tools whose
    /// upstream annotation is missing or wrong.
    #[serde(default)]
    pub sidecar_spec: Option<PathBuf>,
    /// Whether to spawn this upstream. Disabled entries are skipped.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
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
    #[serde(default)]
    pub description: Option<String>,
    /// Override for upstream `execution.taskSupport`. When set, controls whether
    /// the tool appears on vmcp's `run_task` allowlist (`optional`/`required`).
    /// `forbidden` (or omitting with upstream also forbidden) keeps it GraphQL-only.
    #[serde(default)]
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
}

/// Expand `${VAR}` / `$VAR` placeholders from the process environment.
/// Unset variables become an empty string. Used for HTTP upstream `url` /
/// `bearer` so secrets stay in `.env` instead of `registry.json`.
pub fn expand_env(input: &str) -> String {
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
                    out.push_str(&std::env::var(key).unwrap_or_default());
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
            out.push_str(&std::env::var(key).unwrap_or_default());
            i += 1 + len;
        } else {
            out.push('$');
            i += 1;
        }
    }
    out
}

fn expand_upstream(spec: &mut UpstreamSpec) {
    if let Some(url) = spec.url.as_mut() {
        *url = expand_env(url);
    }
    if let Some(bearer) = spec.bearer.as_mut() {
        let expanded = expand_env(bearer);
        if expanded.is_empty() {
            spec.bearer = None;
        } else {
            *bearer = expanded;
        }
    }
    for v in spec.env.values_mut() {
        *v = expand_env(v);
    }
}

/// Load the registry JSON. Returns an empty registry if the file is absent —
/// vmcp can boot with no upstreams (useful for OAuth-only deploys).
///
/// After parse, expands `${ENV}` placeholders in each upstream's `url`,
/// `bearer`, and `env` values.
pub fn load_registry(path: &Path) -> Result<Registry, RegistryError> {
    if !path.exists() {
        tracing::warn!(
            ?path,
            "registry file not found, starting with empty upstream list"
        );
        return Ok(Registry::default());
    }
    let text = fs::read_to_string(path)?;
    let mut registry: Registry = serde_json::from_str(&text)?;
    for upstream in &mut registry.upstreams {
        expand_upstream(upstream);
    }
    Ok(registry)
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
            expand_env("https://ex/${VMCP_TEST_EXPAND_A}/mcp"),
            "https://ex/alpha/mcp"
        );
        assert_eq!(expand_env("tok-$VMCP_TEST_EXPAND_B-end"), "tok-beta-end");
        assert_eq!(expand_env("no/${VMCP_TEST_EXPAND_MISSING}/x"), "no//x");
        std::env::remove_var("VMCP_TEST_EXPAND_A");
        std::env::remove_var("VMCP_TEST_EXPAND_B");
    }

    #[test]
    fn load_registry_expands_bearer_and_drops_empty() {
        let p = tmp_path("reg-expand");
        std::env::set_var("VMCP_TEST_BEARER", "secret-tok");
        fs::write(
            &p,
            r#"{"upstreams":[
              {"name":"a","transport":"http","url":"https://ex/${VMCP_TEST_BEARER}/mcp","bearer":"${VMCP_TEST_BEARER}"},
              {"name":"b","transport":"http","url":"https://ex/mcp","bearer":"${VMCP_TEST_MISSING}"}
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
}
