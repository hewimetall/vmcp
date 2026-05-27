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
pub struct Registry {
    /// Backwards-compat: Python registry uses `servers`, new format uses
    /// `upstreams`. Read both, write the canonical name.
    #[serde(alias = "servers", rename = "upstreams")]
    pub upstreams: Vec<UpstreamSpec>,
}

/// One upstream stdio MCP server entry.
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
    /// Executable to spawn.
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
        Self { version: 1, generated_at: Utc::now(), entries }
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
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Load the registry JSON. Returns an empty registry if the file is absent —
/// vmcp can boot with no upstreams (useful for OAuth-only deploys).
pub fn load_registry(path: &Path) -> Result<Registry, RegistryError> {
    if !path.exists() {
        tracing::warn!(?path, "registry file not found, starting with empty upstream list");
        return Ok(Registry::default());
    }
    let text = fs::read_to_string(path)?;
    let registry: Registry = serde_json::from_str(&text)?;
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

/// Shape-only drift check: do the tool names + JSON schemas + read_only match?
/// Description differences are ignored (descriptions change frequently and
/// don't break the schema).
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
                if s.input_schema != tool.input_schema || s.read_only != tool.read_only {
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
    let Some(sc) = sidecar else { return (tools, Vec::new()); };
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
    fn registry_accepts_both_servers_and_upstreams_keys() {
        let with_servers = r#"{"servers": [{"name": "x", "command": "echo"}]}"#;
        let with_upstreams = r#"{"upstreams": [{"name": "x", "command": "echo"}]}"#;
        let r1: Registry = serde_json::from_str(with_servers).unwrap();
        let r2: Registry = serde_json::from_str(with_upstreams).unwrap();
        assert_eq!(r1.upstreams.len(), 1);
        assert_eq!(r2.upstreams.len(), 1);
        assert_eq!(r1.upstreams[0].name, "x");
    }

    #[test]
    fn detect_drift_picks_up_tool_addition() {
        let stored = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            read_only: true,
        }];
        let live = vec![
            CachedTool {
                name: "a".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                read_only: true,
            },
            CachedTool {
                name: "b".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                read_only: false,
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
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            read_only: true,
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
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: Some("new".into()),
            input_schema: json!({}),
            read_only: true,
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
        }];
        let live = vec![CachedTool {
            name: "a".into(),
            description: None,
            input_schema: json!({}),
            read_only: false,
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
        }];
        let sc = SidecarSpec {
            server: "x".into(),
            tools: vec![SidecarTool {
                name: "danger".into(),
                read_only: false, // …operator overrides it.
                description: None,
            }],
        };
        let (out, _audit) = apply_sidecar(tools, Some(&sc));
        assert!(!out[0].read_only);
    }
}
