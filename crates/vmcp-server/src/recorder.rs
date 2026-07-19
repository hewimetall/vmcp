//! Per-session JSON-RPC dump recorder. Each (client_id, session_id) gets a
//! tokio task owning a JSONL file. mpsc backpressure drops on Full.
//! Broadcast channel fans out to live SSE subscribers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Direction {
    C2S,
    S2C,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    Request,
    Response,
    Notification,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpExchange {
    pub seq: u64,
    pub client_id: Option<String>,
    pub session_id: Option<String>,
    pub timestamp_ms: u64,
    pub direction: Direction,
    pub kind: Kind,
    pub method: Option<String>,
    pub jsonrpc_id: Option<Value>,
    pub body: Value,
    pub latency_ms: Option<u64>,
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub client_id: String,
    pub client_name: Option<String>,
    pub session_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub request_count: u64,
    pub byte_size: u64,
    pub status: String,
    /// Which vmcp endpoint this session attached to — `/mcp` (semantic
    /// GraphQL surface) or `/mcp-proxy` (transparent passthrough). Captured
    /// from the first `McpExchange.upstream` the writer sees. `#[serde(default)]`
    /// keeps old meta files (pre-tag) deserializable.
    #[serde(default)]
    pub upstream: Option<String>,
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct SessionKey {
    pub client_id: String,
    pub session_id: String,
}

pub struct Recorder {
    pub root: PathBuf,
    pub redact_keys: Vec<String>,
    seq: AtomicU64,
    writers: DashMap<SessionKey, mpsc::Sender<McpExchange>>,
    broadcasts: DashMap<SessionKey, broadcast::Sender<Arc<McpExchange>>>,
    pending: DashMap<(String, String), Instant>, // (session_id, jsonrpc_id_serialized)
    dropped: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Recorder {
    pub fn new(root: PathBuf, redact_keys: Vec<String>) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&root);
        Arc::new(Self {
            root,
            redact_keys,
            seq: AtomicU64::new(1),
            writers: DashMap::new(),
            broadcasts: DashMap::new(),
            pending: DashMap::new(),
            dropped: AtomicU64::new(0),
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn record(&self, mut ex: McpExchange) {
        // redact body
        redact_value(&mut ex.body, &self.redact_keys);
        // latency correlation
        if let (Some(sid), Some(id)) = (&ex.session_id, &ex.jsonrpc_id) {
            let key = (sid.clone(), id.to_string());
            match ex.kind {
                Kind::Request => {
                    self.pending.insert(key, Instant::now());
                }
                Kind::Response | Kind::Error => {
                    if let Some((_, t)) = self.pending.remove(&key) {
                        ex.latency_ms = Some(t.elapsed().as_millis() as u64);
                    }
                }
                Kind::Notification => {}
            }
        }
        // route to writer (only if we have both ids)
        let Some(cid) = ex.client_id.clone() else {
            return;
        };
        let Some(sid) = ex.session_id.clone() else {
            return;
        };
        let key = SessionKey {
            client_id: cid,
            session_id: sid,
        };
        let root = self.root.clone();
        let tx = self
            .writers
            .entry(key.clone())
            .or_insert_with(|| spawn_writer(root, key.clone()))
            .clone();
        match tx.try_send(ex.clone()) {
            Ok(()) => {}
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    dropped = self.dropped.load(Ordering::Relaxed),
                    "recorder dropped"
                );
            }
        }
        // fan-out
        let bc = self
            .broadcasts
            .entry(key)
            .or_insert_with(|| broadcast::channel(64).0)
            .clone();
        let _ = bc.send(Arc::new(ex));
    }

    pub fn subscribe(&self, key: &SessionKey) -> broadcast::Receiver<Arc<McpExchange>> {
        self.broadcasts
            .entry(key.clone())
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    pub fn dump_path(&self, client_id: &str, session_id: &str) -> Option<PathBuf> {
        let re = regex::Regex::new(r"^[a-zA-Z0-9_-]{1,128}$").unwrap();
        if !re.is_match(client_id) || !re.is_match(session_id) {
            return None;
        }
        Some(
            self.root
                .join(client_id)
                .join(format!("{session_id}.jsonl")),
        )
    }

    pub async fn list_client_sessions(&self, client_id: &str) -> std::io::Result<Vec<SessionMeta>> {
        let dir = self.root.join(client_id);
        if !dir.is_dir() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(".meta.json"))
                    .unwrap_or(false)
            {
                if let Ok(bytes) = tokio::fs::read(&p).await {
                    if let Ok(m) = serde_json::from_slice::<SessionMeta>(&bytes) {
                        out.push(m);
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn list_all_clients_with_meta(
        &self,
    ) -> std::io::Result<Vec<(String, Vec<SessionMeta>)>> {
        if !self.root.is_dir() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        let mut rd = tokio::fs::read_dir(&self.root).await?;
        while let Some(ent) = rd.next_entry().await? {
            if !ent.file_type().await?.is_dir() {
                continue;
            }
            let cid = ent.file_name().to_string_lossy().to_string();
            let metas = self.list_client_sessions(&cid).await?;
            out.push((cid, metas));
        }
        Ok(out)
    }

    pub async fn startup_cleanup(&self) -> std::io::Result<()> {
        let all = self.list_all_clients_with_meta().await?;
        for (cid, metas) in all {
            for m in metas {
                if m.status == "active" {
                    let path = self
                        .root
                        .join(&cid)
                        .join(format!("{}.meta.json", m.session_id));
                    let fixed = SessionMeta {
                        ended_at_ms: m.ended_at_ms.or(Some(m.started_at_ms)),
                        status: "closed".into(),
                        // Preserve `upstream` and every other field via FRU.
                        ..m
                    };
                    if let Ok(bytes) = serde_json::to_vec_pretty(&fixed) {
                        let _ = tokio::fs::write(&path, bytes).await;
                    }
                }
            }
        }
        Ok(())
    }
}

fn spawn_writer(root: PathBuf, key: SessionKey) -> mpsc::Sender<McpExchange> {
    let (tx, mut rx) = mpsc::channel::<McpExchange>(256);
    tokio::spawn(async move {
        let dir = root.join(&key.client_id);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::error!(?e, ?dir, "create_dir_all failed");
            return;
        }
        let jsonl = dir.join(format!("{}.jsonl", key.session_id));
        let meta_path = dir.join(format!("{}.meta.json", key.session_id));
        let mut f = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(?e, ?jsonl, "open failed");
                return;
            }
        };

        let started_at_ms = now_ms();
        let mut last_seen_ms = started_at_ms;
        let mut request_count: u64 = 0;
        let mut client_name: Option<String> = None;
        let mut upstream: Option<String> = None;

        while let Some(ex) = rx.recv().await {
            if matches!(ex.kind, Kind::Request) {
                request_count += 1;
            }
            last_seen_ms = ex.timestamp_ms;
            if client_name.is_none() {
                client_name = ex
                    .body
                    .get("params")
                    .and_then(|p| p.get("clientInfo"))
                    .and_then(|c| c.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from);
            }
            if upstream.is_none() {
                upstream = ex.upstream.clone();
            }
            let mut line = match serde_json::to_vec(&ex) {
                Ok(b) => b,
                Err(_) => continue,
            };
            line.push(b'\n');
            if let Err(e) = f.write_all(&line).await {
                tracing::error!(?e, "writer flush failed");
                break;
            }
            let _ = f.flush().await;
            // Flush meta on the first request (so the upstream tag, client
            // name, and started_at appear in the admin UI as soon as a
            // session attaches — important for the demo where we look at
            // live sessions before they hit 16 round-trips) and then every
            // 16 thereafter.
            if request_count == 1 || request_count.is_multiple_of(16) {
                let _ = write_meta(
                    &meta_path,
                    &key,
                    client_name.as_deref(),
                    started_at_ms,
                    last_seen_ms,
                    None,
                    request_count,
                    jsonl_size(&jsonl).await,
                    "active",
                    upstream.as_deref(),
                )
                .await;
            }
        }
        let _ = write_meta(
            &meta_path,
            &key,
            client_name.as_deref(),
            started_at_ms,
            last_seen_ms,
            Some(last_seen_ms),
            request_count,
            jsonl_size(&jsonl).await,
            "closed",
            upstream.as_deref(),
        )
        .await;
    });
    tx
}

async fn jsonl_size(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn write_meta(
    path: &Path,
    key: &SessionKey,
    client_name: Option<&str>,
    started: u64,
    last_seen: u64,
    ended: Option<u64>,
    count: u64,
    byte_size: u64,
    status: &str,
    upstream: Option<&str>,
) -> std::io::Result<()> {
    let _ = last_seen; // currently unused; reserved for future fields
    let m = SessionMeta {
        client_id: key.client_id.clone(),
        client_name: client_name.map(String::from),
        session_id: key.session_id.clone(),
        started_at_ms: started,
        ended_at_ms: ended,
        request_count: count,
        byte_size,
        status: status.into(),
        upstream: upstream.map(String::from),
    };
    let bytes = serde_json::to_vec_pretty(&m).unwrap();
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

fn redact_value(v: &mut Value, keys: &[String]) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if keys.iter().any(|rk| rk.eq_ignore_ascii_case(k)) {
                    *val = Value::String("<redacted>".into());
                } else {
                    redact_value(val, keys);
                }
            }
        }
        Value::Array(arr) => {
            for el in arr.iter_mut() {
                redact_value(el, keys);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_top_level() {
        let mut v = json!({"password":"hunter2","ok":"keep"});
        redact_value(&mut v, &["password".into()]);
        assert_eq!(v["password"], "<redacted>");
        assert_eq!(v["ok"], "keep");
    }
    #[test]
    fn redact_nested_and_arrays() {
        let mut v =
            json!({"a":{"Authorization":"Bearer xyz","x":1},"b":[{"token":"t"},{"safe":1}]});
        redact_value(&mut v, &["Authorization".into(), "token".into()]);
        assert_eq!(v["a"]["Authorization"], "<redacted>");
        assert_eq!(v["a"]["x"], 1);
        assert_eq!(v["b"][0]["token"], "<redacted>");
        assert_eq!(v["b"][1]["safe"], 1);
    }
    #[test]
    fn redact_case_insensitive() {
        let mut v = json!({"AUTHORIZATION":"x"});
        redact_value(&mut v, &["authorization".into()]);
        assert_eq!(v["AUTHORIZATION"], "<redacted>");
    }
}
