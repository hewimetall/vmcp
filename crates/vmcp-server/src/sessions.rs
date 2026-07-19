//! MCP session registry. Tracks `Mcp-Session-Id` → metadata for the admin
//! sessions view. Filled by the /mcp middleware.
//!
//! When opened with a root directory, each session is stored as JSON under
//! `{root}/.registry/{session_id}.json` so the list survives gateway restarts.
//! In-memory mode (`SessionRegistry::new`) remains for unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};

fn id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]{1,128}$").unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Closed,
}

#[derive(Debug)]
pub struct SessionInfo {
    pub id: String,
    pub client_id: Mutex<Option<String>>,
    pub client_name: Mutex<Option<String>>,
    pub started_at_ms: u64,
    pub last_seen_ms: AtomicU64,
    pub request_count: AtomicU64,
    pub status: Mutex<SessionStatus>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SessionDisk {
    id: String,
    client_id: Option<String>,
    client_name: Option<String>,
    started_at_ms: u64,
    last_seen_ms: u64,
    request_count: u64,
    status: SessionStatus,
}

#[derive(Serialize, Clone)]
pub struct SessionSnapshot {
    pub id: String,
    pub client_id: Option<String>,
    pub client_name: Option<String>,
    pub started_at_ms: u64,
    pub last_seen_ms: u64,
    pub request_count: u64,
    pub duration_ms: u64,
    pub status: SessionStatus,
}

pub struct SessionRegistry {
    inner: DashMap<String, SessionInfo>,
    /// When set, every mutation is mirrored to `{root}/.registry/{id}.json`.
    root: Option<PathBuf>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn registry_dir(root: &Path) -> PathBuf {
    root.join(".registry")
}

fn session_path(root: &Path, session_id: &str) -> PathBuf {
    registry_dir(root).join(format!("{session_id}.json"))
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    /// In-memory only (no disk). Used by unit/integration tests.
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            root: None,
        }
    }

    /// Load existing sessions from `{root}/.registry/*.json` and persist
    /// subsequent updates there. Missing/unreadable files are skipped.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let dir = registry_dir(&root);
        std::fs::create_dir_all(&dir)?;
        let reg = Self {
            inner: DashMap::new(),
            root: Some(root.clone()),
        };
        for ent in std::fs::read_dir(&dir)?.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(disk) = serde_json::from_slice::<SessionDisk>(&bytes) else {
                tracing::warn!(?path, "skip corrupt session registry json");
                continue;
            };
            if !id_re().is_match(&disk.id) {
                continue;
            }
            reg.inner.insert(
                disk.id.clone(),
                SessionInfo {
                    id: disk.id,
                    client_id: Mutex::new(disk.client_id),
                    client_name: Mutex::new(disk.client_name),
                    started_at_ms: disk.started_at_ms,
                    last_seen_ms: AtomicU64::new(disk.last_seen_ms),
                    request_count: AtomicU64::new(disk.request_count),
                    status: Mutex::new(disk.status),
                },
            );
        }
        Ok(reg)
    }

    fn persist(&self, session_id: &str) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        let Some(s) = self.inner.get(session_id) else {
            return;
        };
        let disk = SessionDisk {
            id: s.id.clone(),
            client_id: s.client_id.lock().clone(),
            client_name: s.client_name.lock().clone(),
            started_at_ms: s.started_at_ms,
            last_seen_ms: s.last_seen_ms.load(Ordering::Relaxed),
            request_count: s.request_count.load(Ordering::Relaxed),
            status: *s.status.lock(),
        };
        let path = session_path(root, session_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&disk) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, &path) {
                tracing::warn!(?e, ?path, "session registry rename failed");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    pub fn record_request(
        &self,
        session_id: &str,
        client_id: Option<&str>,
        client_name: Option<&str>,
    ) {
        if !id_re().is_match(session_id) {
            tracing::warn!(session_id, "invalid id, skipping");
            return;
        }
        let entry = self
            .inner
            .entry(session_id.to_string())
            .or_insert_with(|| SessionInfo {
                id: session_id.to_string(),
                client_id: Mutex::new(None),
                client_name: Mutex::new(None),
                started_at_ms: now_ms(),
                last_seen_ms: AtomicU64::new(now_ms()),
                request_count: AtomicU64::new(0),
                status: Mutex::new(SessionStatus::Active),
            });
        entry.last_seen_ms.store(now_ms(), Ordering::Relaxed);
        entry.request_count.fetch_add(1, Ordering::Relaxed);
        if let Some(c) = client_id {
            let mut g = entry.client_id.lock();
            if g.is_none() {
                *g = Some(c.to_string());
            }
        }
        if let Some(c) = client_name {
            let mut g = entry.client_name.lock();
            if g.is_none() {
                *g = Some(c.to_string());
            }
        }
        // Re-activate if a previously closed session is seen again.
        {
            let mut st = entry.status.lock();
            if *st == SessionStatus::Closed {
                *st = SessionStatus::Active;
            }
        }
        drop(entry);
        self.persist(session_id);
    }

    pub fn close(&self, session_id: &str) {
        if let Some(s) = self.inner.get(session_id) {
            *s.status.lock() = SessionStatus::Closed;
            drop(s);
            self.persist(session_id);
        }
    }

    pub fn gc(&self, now: u64, idle_ttl_ms: u64) {
        let mut closed = Vec::new();
        for s in self.inner.iter() {
            let last = s.last_seen_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) > idle_ttl_ms {
                let mut g = s.status.lock();
                if *g == SessionStatus::Active {
                    *g = SessionStatus::Closed;
                    closed.push(s.id.clone());
                }
            }
        }
        for id in closed {
            self.persist(&id);
        }
    }

    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        let now = now_ms();
        self.inner
            .iter()
            .map(|s| {
                let last_seen = s.last_seen_ms.load(Ordering::Relaxed);
                SessionSnapshot {
                    id: s.id.clone(),
                    client_id: s.client_id.lock().clone(),
                    client_name: s.client_name.lock().clone(),
                    started_at_ms: s.started_at_ms,
                    last_seen_ms: last_seen,
                    request_count: s.request_count.load(Ordering::Relaxed),
                    duration_ms: now.saturating_sub(s.started_at_ms),
                    status: *s.status.lock(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn valid_id_creates_entry() {
        let r = SessionRegistry::new();
        r.record_request("sess-abc123", Some("vmcp-cid"), Some("inspector"));
        let snaps = r.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].client_id.as_deref(), Some("vmcp-cid"));
    }

    #[test]
    fn invalid_id_is_noop() {
        let r = SessionRegistry::new();
        r.record_request("../etc", None, None);
        r.record_request("", None, None);
        r.record_request("with spaces", None, None);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn gc_closes_idle() {
        let r = SessionRegistry::new();
        r.record_request("sess1", None, None);
        for s in r.inner.iter() {
            s.last_seen_ms.store(0, Ordering::Relaxed);
        }
        r.gc(now_ms(), 1000);
        let snaps = r.snapshot();
        assert_eq!(snaps[0].status, SessionStatus::Closed);
    }

    #[test]
    fn survives_reopen_from_json_dir() {
        let dir = tempdir().unwrap();
        let id = {
            let r = SessionRegistry::open(dir.path()).unwrap();
            r.record_request("sess-persist", Some("cid-1"), Some("demo"));
            r.record_request("sess-persist", Some("cid-1"), None);
            let path = session_path(dir.path(), "sess-persist");
            assert!(path.is_file(), "expected {}", path.display());
            "sess-persist".to_string()
        };
        let r2 = SessionRegistry::open(dir.path()).unwrap();
        let snaps = r2.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, id);
        assert_eq!(snaps[0].client_id.as_deref(), Some("cid-1"));
        assert_eq!(snaps[0].client_name.as_deref(), Some("demo"));
        assert_eq!(snaps[0].request_count, 2);
        assert_eq!(snaps[0].status, SessionStatus::Active);
    }

    #[test]
    fn close_and_gc_persist_to_disk() {
        let dir = tempdir().unwrap();
        let r = SessionRegistry::open(dir.path()).unwrap();
        r.record_request("s1", Some("c"), None);
        r.close("s1");
        let r2 = SessionRegistry::open(dir.path()).unwrap();
        assert_eq!(r2.snapshot()[0].status, SessionStatus::Closed);

        let r3 = SessionRegistry::open(dir.path()).unwrap();
        r3.record_request("s2", None, None);
        for s in r3.inner.iter() {
            s.last_seen_ms.store(0, Ordering::Relaxed);
        }
        r3.gc(now_ms(), 1);
        let r4 = SessionRegistry::open(dir.path()).unwrap();
        let s2 = r4.snapshot().into_iter().find(|s| s.id == "s2").unwrap();
        assert_eq!(s2.status, SessionStatus::Closed);
    }

    #[test]
    fn default_is_empty_memory_registry() {
        let r = SessionRegistry::default();
        assert!(r.snapshot().is_empty());
        r.close("missing");
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn open_skips_corrupt_non_json_and_invalid_ids() {
        let dir = tempdir().unwrap();
        let reg_dir = registry_dir(dir.path());
        std::fs::create_dir_all(&reg_dir).unwrap();
        std::fs::write(reg_dir.join("notes.txt"), b"not json").unwrap();
        std::fs::write(reg_dir.join("bad.json"), b"{not-json").unwrap();
        std::fs::write(
            reg_dir.join("evil.json"),
            br#"{"id":"../evil","client_id":null,"client_name":null,"started_at_ms":1,"last_seen_ms":1,"request_count":0,"status":"active"}"#,
        )
        .unwrap();
        // Unreadable path: a nested directory named *.json is skipped by read failure
        // or by extension check — use empty object missing required fields.
        std::fs::write(reg_dir.join("incomplete.json"), br#"{"id":"ok"}"#).unwrap();
        let r = SessionRegistry::open(dir.path()).unwrap();
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn open_fails_when_root_cannot_be_created() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let err = SessionRegistry::open(&blocker);
        assert!(err.is_err());
    }

    #[test]
    fn closed_session_reactivates_and_keeps_first_client() {
        let dir = tempdir().unwrap();
        let r = SessionRegistry::open(dir.path()).unwrap();
        r.record_request("s1", Some("first"), Some("A"));
        r.close("s1");
        assert_eq!(r.snapshot()[0].status, SessionStatus::Closed);
        r.record_request("s1", Some("second"), Some("B"));
        let snap = r.snapshot();
        assert_eq!(snap[0].status, SessionStatus::Active);
        assert_eq!(snap[0].client_id.as_deref(), Some("first"));
        assert_eq!(snap[0].client_name.as_deref(), Some("A"));
        // Persist reactivation
        let r2 = SessionRegistry::open(dir.path()).unwrap();
        assert_eq!(r2.snapshot()[0].status, SessionStatus::Active);
    }

    #[test]
    fn gc_leaves_already_closed_alone() {
        let r = SessionRegistry::new();
        r.record_request("s1", None, None);
        r.close("s1");
        for s in r.inner.iter() {
            s.last_seen_ms.store(0, Ordering::Relaxed);
        }
        r.gc(now_ms(), 1);
        assert_eq!(r.snapshot()[0].status, SessionStatus::Closed);
        assert_eq!(r.snapshot()[0].request_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_skips_unreadable_json() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let reg_dir = registry_dir(dir.path());
        std::fs::create_dir_all(&reg_dir).unwrap();
        let path = reg_dir.join("secret.json");
        std::fs::write(
            &path,
            br#"{"id":"secret","client_id":null,"client_name":null,"started_at_ms":1,"last_seen_ms":1,"request_count":0,"status":"active"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = SessionRegistry::open(dir.path()).unwrap();
        assert!(r.snapshot().is_empty());
        // Restore so tempdir cleanup can remove the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
