//! Local-directory OpenTelemetry span exporter (`otel` feature).
//!
//! MCP middleware emits `tracing` spans → `tracing-opentelemetry` → this
//! exporter appends one [`StoredSpan`] JSON line per span under
//! `{root}/{client_id}/{session_id}.jsonl` and refreshes `*.meta.json`.

use std::collections::HashMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};

use crate::recorder::{SessionKey, SessionMeta};

/// One completed MCP span as stored on disk (one JSONL line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSpan {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    pub start_time_unix_ms: u64,
    pub end_time_unix_ms: u64,
    pub attributes: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl StoredSpan {
    pub fn from_span_data(span: &SpanData) -> Option<Self> {
        let attrs = attrs_to_map(&span.attributes);
        let client_id = attr_str(&attrs, "mcp.client_id")?;
        let session_id = attr_str(&attrs, "mcp.session_id")?;
        if client_id.is_empty() || session_id.is_empty() {
            return None;
        }
        let parent = span.parent_span_id.to_string();
        let parent_span_id = if parent.chars().all(|c| c == '0') {
            None
        } else {
            Some(parent)
        };
        Some(Self {
            trace_id: span.span_context.trace_id().to_string(),
            span_id: span.span_context.span_id().to_string(),
            parent_span_id,
            name: span.name.to_string(),
            start_time_unix_ms: system_time_ms(span.start_time),
            end_time_unix_ms: system_time_ms(span.end_time),
            attributes: attrs,
            status: match &span.status {
                opentelemetry::trace::Status::Unset => None,
                opentelemetry::trace::Status::Ok => Some("ok".into()),
                opentelemetry::trace::Status::Error { description } => {
                    Some(format!("error:{description}"))
                }
            },
        })
    }

    pub fn client_id(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.client_id")
    }

    pub fn session_id(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.session_id")
    }

    pub fn endpoint(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.endpoint")
    }

    pub fn direction(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.direction")
    }

    pub fn kind(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.kind")
    }

    pub fn method(&self) -> Option<&str> {
        attr_str(&self.attributes, "mcp.method")
    }
}

fn attrs_to_map(attrs: &[KeyValue]) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    for kv in attrs {
        let key = kv.key.as_str().to_string();
        let val = match &kv.value {
            opentelemetry::Value::Bool(b) => Value::Bool(*b),
            opentelemetry::Value::I64(i) => Value::Number((*i).into()),
            opentelemetry::Value::F64(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            opentelemetry::Value::String(s) => {
                let s = s.as_str();
                serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
            }
            other => Value::String(other.to_string()),
        };
        out.insert(key, val);
    }
    out
}

fn attr_str<'a>(attrs: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    attrs.get(key).and_then(|v| v.as_str())
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// OpenTelemetry [`SpanExporter`] that partitions MCP spans into per-session
/// JSONL files under a recorder root directory.
#[derive(Clone)]
pub struct DirSpanExporter {
    store: Arc<SpanStore>,
    shut: Arc<AtomicBool>,
}

impl Debug for DirSpanExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirSpanExporter")
            .field("root", &self.store.root)
            .finish()
    }
}

impl DirSpanExporter {
    pub fn new(store: Arc<SpanStore>) -> Self {
        Self {
            store,
            shut: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SpanExporter for DirSpanExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let store = self.store.clone();
        let shut = self.shut.clone();
        async move {
            if shut.load(Ordering::Relaxed) {
                return Err(OTelSdkError::AlreadyShutdown);
            }
            for span in batch {
                let Some(stored) = StoredSpan::from_span_data(&span) else {
                    continue;
                };
                store.ingest(stored).await;
            }
            Ok(())
        }
    }

    fn shutdown_with_timeout(&mut self, _timeout: Duration) -> OTelSdkResult {
        self.shut.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn set_resource(&mut self, _resource: &Resource) {}
}

/// On-disk + live fan-out store for [`StoredSpan`] files.
pub struct SpanStore {
    pub root: PathBuf,
    pub redact_keys: Vec<String>,
    writers: dashmap::DashMap<SessionKey, mpsc::Sender<StoredSpan>>,
    broadcasts: dashmap::DashMap<SessionKey, broadcast::Sender<Arc<StoredSpan>>>,
    dropped: std::sync::atomic::AtomicU64,
}

impl SpanStore {
    pub fn new(root: PathBuf, redact_keys: Vec<String>) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&root);
        Arc::new(Self {
            root,
            redact_keys,
            writers: dashmap::DashMap::new(),
            broadcasts: dashmap::DashMap::new(),
            dropped: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub async fn ingest(&self, mut span: StoredSpan) {
        redact_attrs(&mut span.attributes, &self.redact_keys);
        let Some(cid) = span.client_id().map(str::to_owned) else {
            return;
        };
        let Some(sid) = span.session_id().map(str::to_owned) else {
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
        match tx.try_send(span.clone()) {
            Ok(()) => {}
            Err(_) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    dropped = self.dropped.load(std::sync::atomic::Ordering::Relaxed),
                    "span store dropped"
                );
            }
        }
        let bc = self
            .broadcasts
            .entry(key)
            .or_insert_with(|| broadcast::channel(64).0)
            .clone();
        let _ = bc.send(Arc::new(span));
    }

    pub fn subscribe(&self, key: &SessionKey) -> broadcast::Receiver<Arc<StoredSpan>> {
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
            let is_meta = p
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".meta.json"))
                .unwrap_or(false);
            if is_meta {
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

    pub async fn load_spans(&self, path: &Path) -> Vec<StoredSpan> {
        let Ok(content) = tokio::fs::read_to_string(path).await else {
            return vec![];
        };
        let mut out = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(span) = serde_json::from_str::<StoredSpan>(line) {
                out.push(span);
            }
        }
        out
    }
}

fn redact_attrs(attrs: &mut HashMap<String, Value>, keys: &[String]) {
    for (k, v) in attrs.iter_mut() {
        if keys.iter().any(|rk| rk.eq_ignore_ascii_case(k)) {
            *v = Value::String("<redacted>".into());
        } else {
            redact_value(v, keys);
        }
    }
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

fn spawn_writer(root: PathBuf, key: SessionKey) -> mpsc::Sender<StoredSpan> {
    let (tx, mut rx) = mpsc::channel::<StoredSpan>(256);
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

        while let Some(span) = rx.recv().await {
            if span.kind() == Some("Request") {
                request_count += 1;
            }
            last_seen_ms = span.end_time_unix_ms.max(span.start_time_unix_ms);
            if client_name.is_none() {
                client_name = span
                    .attributes
                    .get("mcp.body")
                    .and_then(|b| b.get("params"))
                    .and_then(|p| p.get("clientInfo"))
                    .and_then(|c| c.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from);
            }
            if upstream.is_none() {
                upstream = span.endpoint().map(str::to_owned);
            }
            let mut line = match serde_json::to_vec(&span) {
                Ok(b) => b,
                Err(_) => continue,
            };
            line.push(b'\n');
            if let Err(e) = f.write_all(&line).await {
                tracing::error!(?e, "writer flush failed");
                break;
            }
            let _ = f.flush().await;
            if request_count == 1 || request_count % 16 == 0 {
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
    let _ = last_seen;
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
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_nested_body() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "mcp.body".into(),
            json!({"params": {"token": "secret", "ok": 1}}),
        );
        redact_attrs(&mut attrs, &["token".into()]);
        assert_eq!(attrs["mcp.body"]["params"]["token"], "<redacted>");
        assert_eq!(attrs["mcp.body"]["params"]["ok"], 1);
    }
}
