//! OTEL-backed MCP capture (`--features otel`).
//!
//! Emits one `tracing` span per JSON-RPC envelope; `tracing-opentelemetry`
//! + [`DirSpanExporter`](vmcp_server::otel_file::DirSpanExporter) persist
//! [`StoredSpan`](vmcp_server::otel_file::StoredSpan) JSONL under sessions_dir.

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
use tracing::{info_span, Instrument};

use vmcp_auth::types::AccessTokenClaims;
use vmcp_server::sessions::SessionRegistry;

const MAX_BODY: usize = 1 << 20;

#[derive(Clone)]
pub struct CaptureState {
    pub registry: Arc<SessionRegistry>,
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
    let client_name_from_req = req_val.as_ref().and_then(extract_client_name);
    let c2s_kind = if req_id.is_some() {
        "Request"
    } else {
        "Notification"
    };
    let c2s_body = req_val.clone();
    let c2s_method = req_method.clone();
    let c2s_id = req_id.clone();

    let req = Request::from_parts(parts, Body::from(req_bytes));
    let started = std::time::Instant::now();
    let resp = next.run(req).await;

    let session_id_out = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|h| h.to_str().ok())
        .map(String::from);
    let final_sid = session_id_out.clone().or(session_id_in);

    if let Some(sid) = final_sid.clone() {
        st.registry
            .record_request(&sid, client_id.as_deref(), client_name_from_req.as_deref());
        if let Some(body_val) = c2s_body {
            emit_span(
                "mcp.client",
                client_id.as_deref(),
                Some(sid.as_str()),
                &st.endpoint,
                "C2S",
                c2s_kind,
                c2s_method.as_deref(),
                c2s_id.as_ref(),
                &body_val,
                None,
            );
        }
    } else {
        tracing::warn!(
            method = ?req_method,
            "C2S exchange has no session id; dropping span"
        );
    }

    let ct = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    if ct.starts_with("text/event-stream") {
        let (parts, body) = resp.into_parts();
        let mut src = body.into_data_stream();
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        tokio::spawn(parse_sse_into_spans(
            rx,
            client_id.clone(),
            final_sid.clone(),
            req_method.clone(),
            st.endpoint.clone(),
            started,
        ));

        let teed = async_stream::stream! {
            while let Some(chunk) = src.next().await {
                if let Ok(bytes) = &chunk {
                    let _ = tx.send(bytes.clone());
                }
                let item: Result<Bytes, axum::Error> = chunk;
                yield item;
            }
        };
        return Response::from_parts(parts, Body::from_stream(teed));
    }

    let (parts, body) = resp.into_parts();
    let resp_bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    let (rsp_method, rsp_id, rsp_val) = parse_envelope(&resp_bytes);
    if let Some(body_val) = rsp_val {
        let is_error = body_val.get("error").is_some();
        let kind = if rsp_id.is_some() {
            if is_error {
                "Error"
            } else {
                "Response"
            }
        } else {
            "Notification"
        };
        emit_span(
            "mcp.server",
            client_id.as_deref(),
            final_sid.as_deref(),
            &st.endpoint,
            "S2C",
            kind,
            rsp_method.or(req_method).as_deref(),
            rsp_id.as_ref(),
            &body_val,
            Some(started.elapsed().as_millis() as u64),
        );
    }

    Response::from_parts(parts, Body::from(resp_bytes))
}

#[allow(clippy::too_many_arguments)]
fn emit_span(
    name: &'static str,
    client_id: Option<&str>,
    session_id: Option<&str>,
    endpoint: &str,
    direction: &str,
    kind: &str,
    method: Option<&str>,
    jsonrpc_id: Option<&Value>,
    body: &Value,
    latency_ms: Option<u64>,
) {
    let body_str = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    let id_str = jsonrpc_id
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()))
        .unwrap_or_else(|| "null".into());
    let span = info_span!(
        "mcp",
        otel.name = name,
        mcp.client_id = client_id.unwrap_or(""),
        mcp.session_id = session_id.unwrap_or(""),
        mcp.endpoint = endpoint,
        mcp.direction = direction,
        mcp.kind = kind,
        mcp.method = method.unwrap_or(""),
        mcp.jsonrpc_id = %id_str,
        mcp.body = %body_str,
        mcp.latency_ms = latency_ms.map(|n| n as i64).unwrap_or(0),
    );
    let _g = span.entered();
}

async fn parse_sse_into_spans(
    mut rx: mpsc::UnboundedReceiver<Bytes>,
    client_id: Option<String>,
    session_id: Option<String>,
    fallback_method: Option<String>,
    endpoint: String,
    started: std::time::Instant,
) {
    let mut buf = String::new();
    while let Some(bytes) = rx.recv().await {
        match std::str::from_utf8(&bytes) {
            Ok(s) => buf.push_str(s),
            Err(_) => continue,
        }
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
                        "Error"
                    } else {
                        "Response"
                    }
                } else {
                    "Notification"
                };
                let fut = async {
                    emit_span(
                        "mcp.server",
                        client_id.as_deref(),
                        session_id.as_deref(),
                        &endpoint,
                        "S2C",
                        kind,
                        method.as_deref(),
                        id.as_ref(),
                        &v,
                        Some(started.elapsed().as_millis() as u64),
                    );
                };
                fut.instrument(info_span!("mcp.sse_frame")).await;
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
