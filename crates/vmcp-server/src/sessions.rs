//! Live MCP session registry. Tracks `Mcp-Session-Id` → metadata, for the
//! admin sessions view. Filled by the /mcp middleware.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;

fn id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]{1,128}$").unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
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
    }

    pub fn close(&self, session_id: &str) {
        if let Some(s) = self.inner.get(session_id) {
            *s.status.lock() = SessionStatus::Closed;
        }
    }

    pub fn gc(&self, now: u64, idle_ttl_ms: u64) {
        for s in self.inner.iter() {
            let last = s.last_seen_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) > idle_ttl_ms {
                let mut g = s.status.lock();
                if *g == SessionStatus::Active {
                    *g = SessionStatus::Closed;
                }
            }
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
        // Force last_seen to ancient time
        for s in r.inner.iter() {
            s.last_seen_ms.store(0, Ordering::Relaxed);
        }
        r.gc(now_ms(), 1000);
        let snaps = r.snapshot();
        assert_eq!(snaps[0].status, SessionStatus::Closed);
    }
}
