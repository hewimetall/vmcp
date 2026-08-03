//! Regression for https://github.com/hewimetall/vmcp/issues/2
//!
//! After a JSON-RPC error on Streamable HTTP, EventSource clients reconnect
//! with `Last-Event-ID` (priming `retry: 3000`). Resume must replay the cached
//! error event — not return an empty stream / `Channel closed`.

mod common;

use std::time::Duration;

fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir.join("skills")).unwrap();
    std::fs::create_dir_all(dir.join("specs")).unwrap();
    std::fs::write(dir.join("registry.json"), br#"{"upstreams":[]}"#).unwrap();
    std::fs::write(
        dir.join("skills/need_video.yaml"),
        r#"
name: need_video
description: skill with a required argument
arguments:
  - name: video
    required: true
template: "Video is {{video}}"
"#,
    )
    .unwrap();

    let config_path = dir.join("vmcp.toml");
    let config = format!(
        r#"
host = "127.0.0.1"
public_base_url = "http://127.0.0.1:8765"
registry_path = "{reg}"
lock_path     = "{lock}"
spec_dir      = "{spec}"
skills_dir    = "{skills}"

[gql]
max_depth = 10
max_complexity = 1000

[upstream]
spawn_timeout_ms = 30000
call_timeout_ms  = 60000

[auth]
enabled = false
master_password_argon2 = ""
jwt_kid = "unused"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

fn sse_event_ids(body: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for chunk in body.split("\n\n") {
        for line in chunk.lines() {
            if let Some(id) = line.strip_prefix("id:") {
                ids.push(id.trim().to_string());
            }
        }
    }
    ids
}

#[tokio::test]
async fn resume_after_jsonrpc_error_replays_cached_event() {
    let dir = common::TempDir::new("vmcp-session-rpc-error");
    let cfg = write_config(dir.path());
    let gw = common::spawn_gateway(&cfg).await;
    let client = reqwest::Client::new();

    let init = client
        .post(&gw.mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "issue-2", "version": "0"}
            }
        }))
        .send()
        .await
        .expect("initialize");
    assert_eq!(init.status(), 200);
    let session_id = init
        .headers()
        .get("mcp-session-id")
        .expect("Mcp-Session-Id")
        .to_str()
        .unwrap()
        .to_string();

    let status = client
        .post(&gw.mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .header("Mcp-Protocol-Version", "2025-11-25")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("initialized")
        .status();
    assert_eq!(status, 202);

    let err_resp = client
        .post(&gw.mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .header("Mcp-Protocol-Version", "2025-11-25")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "prompts/get",
            "params": {"name": "need_video"}
        }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("prompts/get error");
    assert_eq!(err_resp.status(), 200);
    let err_body = err_resp.text().await.unwrap();
    assert!(
        err_body.contains(r#""error""#) && err_body.contains("missing required argument"),
        "expected JSON-RPC error body, got: {err_body}"
    );
    let priming_id = sse_event_ids(&err_body)
        .into_iter()
        .next()
        .expect("priming event id");

    // Mimic EventSource reconnect after SSE `retry:`.
    let resume = client
        .get(&gw.mcp_url)
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .header("Mcp-Protocol-Version", "2025-11-25")
        .header("Last-Event-ID", &priming_id)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("resume");
    assert_eq!(resume.status(), 200);
    let resume_body = resume.text().await.unwrap();
    assert!(
        resume_body.contains(r#""error""#) && resume_body.contains("missing required argument"),
        "resume must replay the cached JSON-RPC error, got: {resume_body:?}"
    );

    // Fresh POST on the same session must still work.
    let list = client
        .post(&gw.mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .header("Mcp-Protocol-Version", "2025-11-25")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/list",
            "params": {}
        }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("tools/list");
    assert_eq!(list.status(), 200);
    let list_body = list.text().await.unwrap();
    assert!(
        list_body.contains("query_graphql"),
        "same session should still serve tools/list, got: {list_body}"
    );
}
