//! Stress: open Streamable HTTP sessions + unread SSE streams against a
//! gateway with a tiny `RLIMIT_NOFILE` until accepts/POSTs fail ("лопнет").
//! Then drop holders, wait idle GC (`close_session`), and prove a new session
//! works again — FDs were released.

mod common;

use std::time::Duration;

fn write_config(
    dir: &std::path::Path,
    idle_ttl_secs: u64,
    gc_interval_secs: u64,
) -> std::path::PathBuf {
    std::fs::create_dir_all(dir.join("skills")).unwrap();
    std::fs::create_dir_all(dir.join("specs")).unwrap();
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(dir.join("registry.json"), br#"{"upstreams":[]}"#).unwrap();

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

[recorder]
sessions_dir = "{sessions}"
idle_ttl_secs = {idle}
gc_interval_secs = {gc}
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
        sessions = dir.join("sessions").display(),
        idle = idle_ttl_secs,
        gc = gc_interval_secs,
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

struct HeldSession {
    session_id: String,
    /// Unread SSE body — keeps a server-side FD / stream alive.
    _sse: reqwest::Response,
}

async fn open_held_session(
    client: &reqwest::Client,
    mcp_url: &str,
    step_timeout: Duration,
) -> Result<HeldSession, String> {
    let init = tokio::time::timeout(
        step_timeout,
        client
            .post(mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "fd-burst", "version": "0"}
                }
            }))
            .send(),
    )
    .await
    .map_err(|_| "initialize timeout".to_string())?
    .map_err(|e| format!("initialize send: {e}"))?;
    if !init.status().is_success() {
        return Err(format!("initialize status {}", init.status()));
    }
    let session_id = init
        .headers()
        .get("mcp-session-id")
        .ok_or_else(|| "missing Mcp-Session-Id".to_string())?
        .to_str()
        .map_err(|e| e.to_string())?
        .to_string();
    let _ = tokio::time::timeout(step_timeout, init.text())
        .await
        .map_err(|_| "initialize body timeout".to_string())?
        .map_err(|e| format!("initialize body: {e}"))?;

    let initd = tokio::time::timeout(
        step_timeout,
        client
            .post(mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .header("Mcp-Protocol-Version", "2025-11-25")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .send(),
    )
    .await
    .map_err(|_| "initialized timeout".to_string())?
    .map_err(|e| format!("initialized: {e}"))?;
    if initd.status() != 202 {
        return Err(format!("initialized status {}", initd.status()));
    }

    // Standalone SSE GET — leave unread to pin an FD on the server.
    // No request timeout: we intentionally hold the body for the storm.
    let sse = tokio::time::timeout(
        step_timeout,
        client
            .get(mcp_url)
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .header("Mcp-Protocol-Version", "2025-11-25")
            .send(),
    )
    .await
    .map_err(|_| "sse get timeout".to_string())?
    .map_err(|e| format!("sse get: {e}"))?;
    if !sse.status().is_success() {
        return Err(format!("sse status {}", sse.status()));
    }

    Ok(HeldSession {
        session_id,
        _sse: sse,
    })
}

#[tokio::test]
async fn session_storm_bursts_nofile_then_gc_recovers() {
    let dir = common::TempDir::new("vmcp-session-fd-burst");
    // Short idle so GC can reclaim after we drop holders.
    let cfg = write_config(dir.path(), 2, 1);

    // Tiny FD budget: listen socket + overhead, then held SSE sessions
    // should exhaust the soft limit.
    const NOFILE: u64 = 64;
    let gw = common::spawn_gateway_limited(
        &cfg,
        true,
        &[("VMCP_SESSION_CHANNEL_CAPACITY", "16")],
        Some(NOFILE),
    )
    .await;

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    let mut held = Vec::new();
    let mut burst_err = None;
    for i in 0..256 {
        match open_held_session(&client, &gw.mcp_url, Duration::from_secs(3)).await {
            Ok(s) => held.push(s),
            Err(e) => {
                eprintln!("burst at session #{i} after {} held: {e}", held.len());
                burst_err = Some(e);
                break;
            }
        }
    }

    let opened = held.len();
    assert!(
        opened > 0,
        "expected to open at least one session before burst"
    );
    assert!(
        burst_err.is_some(),
        "expected FD/session storm to burst under nofile={NOFILE}; opened={opened} without failure"
    );
    assert!(
        opened < 200,
        "storm should hit nofile well before 200 sessions; opened={opened}"
    );

    // Release client-side holders; idle GC must close_session and free FDs.
    drop(held);
    tokio::time::sleep(Duration::from_secs(5)).await;

    open_held_session(&client, &gw.mcp_url, Duration::from_secs(15))
        .await
        .expect("new session after idle GC should work once FDs are released");
}
