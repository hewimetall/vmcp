//! Axum middleware that captures every JSON-RPC envelope flowing through
//! `/mcp` and feeds it to the [`Recorder`] + [`SessionRegistry`]. Sits
//! INSIDE `require_bearer` so we always have the verified
//! [`AccessTokenClaims`] available in request extensions.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header::CONTENT_TYPE, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use vmcp_auth::types::AccessTokenClaims;
use vmcp_server::recorder::{Direction, Kind, McpExchange, Recorder};
use vmcp_server::sessions::SessionRegistry;

const MAX_BODY: usize = 1 << 20;

#[derive(Clone)]
pub struct CaptureState {
    pub recorder: Arc<Recorder>,
    pub registry: Arc<SessionRegistry>,
    /// Mount path of this capture layer, e.g. `/mcp` or `/mcp-proxy`. Stamped
    /// onto every `McpExchange.upstream` so the admin UI can tell the two
    /// endpoints apart when comparing sessions.
    pub endpoint: String,
}

pub async fn capture_mcp(
    State(st): State<CaptureState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let claims = req.extensions().get::<AccessTokenClaims>().cloned();
    let client_id = claims.map(|c| c.client_id);
    let session_id_in = req
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|h| h.to_str().ok())
        .map(String::from);

    let (parts, body) = req.into_parts();
    let req_bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body > 1MiB").into_response(),
    };
    let (req_method, req_id, req_val) = parse_envelope(&req_bytes);

    // Extract clientInfo.name (set on the initial `initialize` request).
    let client_name_from_req = req_val.as_ref().and_then(extract_client_name);

    // Build the C2S exchange but DO NOT record yet. The very first request
    // ("initialize") arrives WITHOUT an `Mcp-Session-Id` header; rmcp
    // generates the id and returns it in the response. We must wait for
    // `final_sid` so we can fill it in before flushing this exchange to
    // disk — otherwise the initialize C2S frame is dropped by the recorder
    // (it requires both client_id + session_id to route to a writer) and
    // `client_name` never makes it into `*.meta.json`.
    let mut pending_c2s = req_val.clone().map(|body_val| McpExchange {
        seq: st.recorder.next_seq(),
        client_id: client_id.clone(),
        session_id: session_id_in.clone(),
        timestamp_ms: now_ms(),
        direction: Direction::C2S,
        kind: if req_id.is_some() {
            Kind::Request
        } else {
            Kind::Notification
        },
        method: req_method.clone(),
        jsonrpc_id: req_id.clone(),
        body: body_val,
        latency_ms: None,
        upstream: Some(st.endpoint.clone()),
    });

    let req = Request::from_parts(parts, Body::from(req_bytes));
    let resp = next.run(req).await;

    let session_id_out = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|h| h.to_str().ok())
        .map(String::from);
    let final_sid = session_id_out.clone().or(session_id_in);

    // Now flush the buffered C2S exchange, backfilling session_id from
    // `final_sid` if the request arrived without one (initialize case).
    if let Some(mut ex) = pending_c2s.take() {
        if ex.session_id.is_none() {
            ex.session_id = final_sid.clone();
        }
        if let Some(sid) = ex.session_id.clone() {
            st.registry.record_request(
                &sid,
                client_id.as_deref(),
                client_name_from_req.as_deref(),
            );
            tracing::debug!(
                method = ?ex.method,
                session_id = %sid,
                "recording C2S after final_sid resolved"
            );
            st.recorder.record(ex).await;
        } else {
            tracing::warn!(
                method = ?req_method,
                "C2S exchange has no session id (initialize failed or broken client); dropping"
            );
        }
    }
    let ct = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    if ct.starts_with("text/event-stream") {
        // SSE response from rmcp. Tee the byte stream:
        //   * one copy → forward to the original client unchanged
        //   * other copy → side-channel mpsc to a parser task that pulls
        //     out `data: {json-rpc}` frames and records each as its own
        //     McpExchange. The parser runs alongside the passthrough so
        //     we never block the client.
        let (parts, body) = resp.into_parts();
        let mut src = body.into_data_stream();

        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        tokio::spawn(parse_sse_into_recorder(
            rx,
            st.recorder.clone(),
            client_id.clone(),
            final_sid.clone(),
            req_method.clone(),
            st.endpoint.clone(),
        ));

        let teed = async_stream::stream! {
            while let Some(chunk) = src.next().await {
                match &chunk {
                    Ok(bytes) => {
                        let _ = tx.send(bytes.clone());
                    }
                    Err(_) => {
                        // Forward the error to the client; parser will see
                        // the channel close and finalize.
                    }
                }
                let item: Result<Bytes, axum::Error> = chunk;
                yield item;
            }
            // tx drops here → parser task exits cleanly.
        };

        return Response::from_parts(parts, Body::from_stream(teed));
    }

    let (parts, body) = resp.into_parts();
    let resp_bytes = axum::body::to_bytes(body, MAX_BODY).await.unwrap_or_default();
    let (rsp_method, rsp_id, rsp_val) = parse_envelope(&resp_bytes);
    if let Some(body_val) = rsp_val {
        let is_error = body_val.get("error").is_some();
        let kind = if rsp_id.is_some() {
            if is_error {
                Kind::Error
            } else {
                Kind::Response
            }
        } else {
            Kind::Notification
        };
        let ex = McpExchange {
            seq: st.recorder.next_seq(),
            client_id,
            session_id: final_sid,
            timestamp_ms: now_ms(),
            direction: Direction::S2C,
            kind,
            method: rsp_method.or(req_method),
            jsonrpc_id: rsp_id,
            body: body_val,
            latency_ms: None,
            upstream: Some(st.endpoint.clone()),
        };
        st.recorder.record(ex).await;
    }

    Response::from_parts(parts, Body::from(resp_bytes))
}

/// Reads byte chunks from an SSE body stream (via the unbounded mpsc
/// fed by the tee), splits them on `\n\n` frame boundaries, extracts
/// `data: {json}` payloads, and records each as a Response/Error/
/// Notification `McpExchange`. Runs in its own task; never blocks the
/// passthrough.
async fn parse_sse_into_recorder(
    mut rx: mpsc::UnboundedReceiver<Bytes>,
    recorder: Arc<Recorder>,
    client_id: Option<String>,
    session_id: Option<String>,
    fallback_method: Option<String>,
    endpoint: String,
) {
    let mut buf = String::new();
    while let Some(bytes) = rx.recv().await {
        match std::str::from_utf8(&bytes) {
            Ok(s) => buf.push_str(s),
            Err(_) => continue, // skip invalid UTF-8 chunks
        }
        // SSE frames are separated by a blank line ("\n\n"). Drain
        // every complete frame currently in the buffer.
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            for line in frame.lines() {
                let payload = match line.strip_prefix("data:") {
                    Some(p) => p.trim_start(),
                    None => continue,
                };
                if payload.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let entry = if v.is_array() {
                    v.as_array()
                        .and_then(|a| a.first().cloned())
                        .unwrap_or_else(|| v.clone())
                } else {
                    v.clone()
                };
                let method = entry
                    .get("method")
                    .and_then(|m| m.as_str())
                    .map(String::from)
                    .or_else(|| fallback_method.clone());
                let id = entry.get("id").cloned();
                let is_error = entry.get("error").is_some();
                let kind = if id.is_some() {
                    if is_error {
                        Kind::Error
                    } else {
                        Kind::Response
                    }
                } else {
                    Kind::Notification
                };
                let ex = McpExchange {
                    seq: recorder.next_seq(),
                    client_id: client_id.clone(),
                    session_id: session_id.clone(),
                    timestamp_ms: now_ms(),
                    direction: Direction::S2C,
                    kind,
                    method,
                    jsonrpc_id: id,
                    body: v,
                    latency_ms: None,
                    upstream: Some(endpoint.clone()),
                };
                recorder.record(ex).await;
            }
        }
    }
}

fn parse_envelope(bytes: &Bytes) -> (Option<String>, Option<Value>, Option<Value>) {
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => {
            return (
                None,
                None,
                Some(Value::String(String::from_utf8_lossy(bytes).into_owned())),
            )
        }
    };
    let entry = if v.is_array() {
        v.as_array()
            .and_then(|a| a.first().cloned())
            .unwrap_or_else(|| v.clone())
    } else {
        v.clone()
    };
    let method = entry
        .get("method")
        .and_then(|m| m.as_str())
        .map(String::from);
    let id = entry.get("id").cloned();
    (method, id, Some(v))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Pull `params.clientInfo.name` out of a JSON-RPC request body. Returns
/// `None` for any other request shape. Used to seed `SessionMeta.client_name`
/// from the very first `initialize` envelope.
fn extract_client_name(v: &Value) -> Option<String> {
    let entry = if v.is_array() {
        v.as_array().and_then(|a| a.first())?
    } else {
        v
    };
    entry
        .get("params")
        .and_then(|p| p.get("clientInfo"))
        .and_then(|c| c.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode as HttpStatusCode, routing::post, Router};
    use serde_json::json;
    use tower::ServiceExt;
    use vmcp_auth::types::AccessTokenClaims;

    fn fake_claims(client_id: &str) -> AccessTokenClaims {
        AccessTokenClaims {
            iss: "test".into(),
            aud: "test".into(),
            sub: client_id.into(),
            client_id: client_id.into(),
            scope: "mcp".into(),
            iat: 0,
            exp: i64::MAX,
            jti: "test-jti".into(),
        }
    }

    /// Inserts a hard-coded `AccessTokenClaims` into request extensions so
    /// `capture_mcp` sees a non-`None` `client_id`. Stands in for the real
    /// `require_bearer` middleware.
    async fn insert_test_claims(
        mut req: Request<Body>,
        next: Next,
    ) -> Response {
        req.extensions_mut().insert(fake_claims("test-client-id"));
        next.run(req).await
    }

    /// Stub handler that mimics rmcp's behavior for the very first
    /// `initialize` request: it MUST set `Mcp-Session-Id` in the response
    /// header (the client did not send one). Returns a JSON-RPC result.
    async fn stub_initialize_handler() -> Response {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "vmcp-test", "version": "0.1"}
            }
        });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut resp = Response::new(Body::from(body_bytes));
        resp.headers_mut()
            .insert("Mcp-Session-Id", "test-session-123".parse().unwrap());
        resp.headers_mut()
            .insert(CONTENT_TYPE, "application/json".parse().unwrap());
        resp
    }

    /// RAII-style temp directory wrapper to avoid pulling in `tempfile`.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("vmcp-capture-test-{nanos}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn extract_client_name_from_initialize() {
        let v = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "claude-code", "version": "0.1"}
            }
        });
        assert_eq!(extract_client_name(&v).as_deref(), Some("claude-code"));
    }

    #[test]
    fn extract_client_name_missing() {
        let v = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        assert_eq!(extract_client_name(&v), None);
    }

    #[test]
    fn extract_client_name_from_batch_array() {
        let v = json!([
            {"jsonrpc":"2.0","id":1,"method":"initialize",
             "params":{"clientInfo":{"name":"batched"}}},
            {"jsonrpc":"2.0","id":2,"method":"tools/list"}
        ]);
        assert_eq!(extract_client_name(&v).as_deref(), Some("batched"));
    }

    /// Regression test for the orphan-C2S bug: the very first `initialize`
    /// request arrives WITHOUT `Mcp-Session-Id` in the request header. The
    /// server (rmcp / stub) generates the session id and returns it in the
    /// RESPONSE header. Previously the middleware recorded the C2S frame
    /// immediately with `session_id=None`, which Recorder silently dropped
    /// — so the initialize request never made it to disk and `client_name`
    /// was never seeded into the meta file. After the fix the C2S exchange
    /// is buffered until `final_sid` resolves, then flushed.
    #[tokio::test]
    async fn captures_initialize_request_via_orphan_buffer() {
        let tmp = TempDir::new();
        let recorder = Recorder::new(tmp.path().to_path_buf(), vec![]);
        let registry = Arc::new(SessionRegistry::new());
        let state = CaptureState {
            recorder: recorder.clone(),
            registry: registry.clone(),
            endpoint: "/mcp".into(),
        };

        let app = Router::new()
            .route("/mcp", post(stub_initialize_handler))
            .layer(axum::middleware::from_fn_with_state(state, capture_mcp))
            .layer(axum::middleware::from_fn(insert_test_claims));

        let req = Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("content-type", "application/json")
            // NOTE: no `Mcp-Session-Id` header — this is what makes it an
            // "orphan" C2S in the old code path.
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {"name": "test-client", "version": "0.1"}
                    }
                }))
                .unwrap(),
            ))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), HttpStatusCode::OK);
        let sid = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|h| h.to_str().ok())
            .map(String::from)
            .expect("stub must have set Mcp-Session-Id");
        assert_eq!(sid, "test-session-123");

        // The registry should have a row keyed by the response-provided sid
        // with client_name pulled from the request body.
        // Poll briefly because writers run on background tasks.
        let snapshot = poll_for(
            || {
                let snaps = registry.snapshot();
                if snaps.is_empty() {
                    None
                } else {
                    Some(snaps)
                }
            },
            std::time::Duration::from_secs(2),
        )
        .await
        .expect("registry must have at least one session");
        let row = snapshot
            .iter()
            .find(|s| s.id == sid)
            .expect("session id must match the response header");
        assert_eq!(row.client_id.as_deref(), Some("test-client-id"));
        assert_eq!(row.client_name.as_deref(), Some("test-client"));

        // The JSONL dump should have at least the C2S initialize frame
        // with method="initialize" and session_id=sid. We don't assert on
        // `*.meta.json` because the recorder writer task only flushes meta
        // every 16 requests OR on channel close — with a single recorded
        // exchange neither condition fires inside the test window. The
        // registry assertions above already cover the `client_name` path
        // (which is what was previously broken by the orphan-C2S bug).
        let jsonl_path = tmp
            .path()
            .join("test-client-id")
            .join(format!("{sid}.jsonl"));
        let dump = poll_for(
            || {
                let bytes = std::fs::read(&jsonl_path).ok()?;
                if bytes.is_empty() {
                    None
                } else {
                    Some(bytes)
                }
            },
            std::time::Duration::from_secs(2),
        )
        .await
        .expect("jsonl must be written");
        let dump_str = String::from_utf8(dump).unwrap();
        let lines: Vec<&str> = dump_str.lines().filter(|l| !l.is_empty()).collect();
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // Must contain exactly the C2S initialize (the non-SSE S2C branch
        // would record the response too, so >= 1 C2S Request and the
        // session_id field set on every entry).
        assert!(
            parsed.iter().any(|e| e["direction"] == "C2S"
                && e["kind"] == "Request"
                && e["method"] == "initialize"),
            "expected a C2S initialize request frame, got {parsed:?}"
        );
        for e in &parsed {
            assert_eq!(
                e["session_id"], sid,
                "every recorded exchange must carry the resolved session_id"
            );
        }
    }

    /// Poll a closure until it returns `Some(_)` or the timeout elapses.
    async fn poll_for<T>(
        mut f: impl FnMut() -> Option<T>,
        max: std::time::Duration,
    ) -> Option<T> {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if let Some(v) = f() {
                return Some(v);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        f()
    }
}
