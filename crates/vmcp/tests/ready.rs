//! `/ready` soft readiness: 503 when registry has enabled upstreams but none connected.

mod common;

use std::time::Duration;

const DEMO_ARGON2: &str = "$argon2id$v=19$m=19456,t=2,p=1$EKXF2yiUMT1injIS9ueldA$1Pra/zoGSKVIkZq1fCg0Hd2ceJuQn1H4k2lXeKUkMD8";

fn write_cfg(dir: &std::path::Path, registry: &str) -> std::path::PathBuf {
    std::fs::write(dir.join("registry.json"), registry.as_bytes()).unwrap();
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::create_dir_all(dir.join("specs")).unwrap();
    std::fs::create_dir_all(dir.join("skills")).unwrap();
    let tokens = dir.join("tokens.json");
    std::fs::write(&tokens, b"[]").unwrap();

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
spawn_timeout_ms = 2000
call_timeout_ms  = 2000

[auth]
enabled = false
master_password_argon2 = "{argon}"
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
        argon = DEMO_ARGON2,
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

#[tokio::test]
async fn ready_ok_when_registry_empty() {
    let dir = common::TempDir::new("vmcp-ready-empty");
    let cfg = write_cfg(dir.path(), r#"{"upstreams":[]}"#);
    let gw = common::spawn_gateway(&cfg).await;
    let url = format!("http://127.0.0.1:{}/ready", gw.port);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap().trim(), "ready");
}

#[tokio::test]
async fn ready_503_when_all_enabled_upstreams_fail_to_spawn() {
    let dir = common::TempDir::new("vmcp-ready-fail");
    // HTTP upstream pointing at a closed port — spawn/handshake fails → empty pool.
    let registry = r#"{
      "upstreams": [
        {
          "name": "dead",
          "transport": "http",
          "url": "http://127.0.0.1:1/mcp",
          "enabled": true
        }
      ]
    }"#;
    let cfg = write_cfg(dir.path(), registry);
    let gw = common::spawn_gateway(&cfg).await;

    // Give spawn attempts a moment (spawn_timeout 2s; boot may still be finishing).
    tokio::time::sleep(Duration::from_millis(500)).await;

    let url = format!("http://127.0.0.1:{}/ready", gw.port);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "body: {:?}",
        resp.text().await
    );
}
