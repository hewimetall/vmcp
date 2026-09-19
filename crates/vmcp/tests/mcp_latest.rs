//! E2E: `[mcp].latest` advertisement on Streamable HTTP `/mcp` and `/mcp-proxy`.
//!
//! Default-off keeps initialize / `server/discover` on the legacy era
//! (through `2025-11-25`). `latest = true` or `VMCP_MCP__LATEST=true` is
//! dual-era: clients may still pick `2025-11-25`, and `2026-07-28` is added
//! to `supportedVersions`.

mod common;

use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};

const LEGACY: &str = "2025-11-25";
const LATEST: &str = "2026-07-28";

fn write_config(dir: &std::path::Path, latest: Option<bool>, proxy: bool) -> std::path::PathBuf {
    std::fs::write(dir.join("registry.json"), br#"{"upstreams":[]}"#).unwrap();
    std::fs::create_dir_all(dir.join("specs")).unwrap();
    std::fs::create_dir_all(dir.join("skills")).unwrap();

    let mcp = match latest {
        Some(true) => "\n[mcp]\nlatest = true\n",
        Some(false) => "\n[mcp]\nlatest = false\n",
        None => "",
    };
    let proxy_block = if proxy {
        "\n[proxy]\nenabled = true\nmcp_path = \"/mcp-proxy\"\n"
    } else {
        ""
    };
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
{mcp}{proxy}
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
        mcp = mcp,
        proxy = proxy_block,
    );
    let path = dir.join("vmcp.toml");
    std::fs::write(&path, config).unwrap();
    path
}

fn parse_jsonrpc(body: &str) -> Value {
    let trimmed = body.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v;
    }
    let mut last = None;
    for line in body.lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            last = Some(v);
        }
    }
    last.unwrap_or_else(|| panic!("expected JSON-RPC body, got: {body}"))
}

async fn post_rpc(
    url: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.expect("POST MCP");
    let status = resp.status();
    let text = resp.text().await.expect("response body");
    (status, parse_jsonrpc(&text))
}

async fn initialize(url: &str, protocol_version: &str) -> (reqwest::StatusCode, Value) {
    post_rpc(
        url,
        &[],
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "mcp-latest-e2e", "version": "0"}
            }
        }),
    )
    .await
}

fn discover_body(protocol_version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": protocol_version,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "mcp-latest-e2e", "version": "0"}
            }
        }
    })
}

async fn discover(url: &str, protocol_version: &str) -> (reqwest::StatusCode, Value) {
    let mut headers = vec![("MCP-Protocol-Version", protocol_version)];
    if protocol_version >= LATEST {
        headers.push(("Mcp-Method", "server/discover"));
    }
    post_rpc(url, &headers, discover_body(protocol_version)).await
}

fn result_protocol_version(msg: &Value) -> &str {
    msg["result"]["protocolVersion"]
        .as_str()
        .unwrap_or_else(|| panic!("initialize result missing protocolVersion: {msg}"))
}

fn supported_versions(msg: &Value) -> Vec<String> {
    msg["result"]["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("discover result missing supportedVersions: {msg}"))
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn latest_off_stays_legacy_on_mcp() {
    let dir = common::TempDir::new("vmcp-latest-off");
    let cfg = write_config(dir.path(), None, false);
    let gw = common::spawn_gateway(&cfg).await;

    let (status, init_2026) = initialize(&gw.mcp_url, LATEST).await;
    assert_eq!(status, 200, "initialize 2026: {init_2026}");
    assert_eq!(
        result_protocol_version(&init_2026),
        LEGACY,
        "latest=false must fall back from 2026-07-28, got {init_2026}"
    );

    let (status, init_legacy) = initialize(&gw.mcp_url, LEGACY).await;
    assert_eq!(status, 200, "initialize legacy: {init_legacy}");
    assert_eq!(result_protocol_version(&init_legacy), LEGACY);

    let (status, disc_2026) = discover(&gw.mcp_url, LATEST).await;
    assert!(
        disc_2026.get("error").is_some(),
        "latest=false must reject server/discover at 2026-07-28, status={status} body={disc_2026}"
    );

    let (status, disc_legacy) = discover(&gw.mcp_url, LEGACY).await;
    assert_eq!(status, 200, "discover legacy: {disc_legacy}");
    let versions = supported_versions(&disc_legacy);
    assert!(
        versions.iter().any(|v| v == LEGACY),
        "legacy discover must list {LEGACY}, got {versions:?}"
    );
    assert!(
        versions.iter().all(|v| v.as_str() < LATEST),
        "latest=false must not advertise {LATEST}, got {versions:?}"
    );

    let client = common::connect_client(NullClient, gw.mcp_url.clone()).await;
    let tools = client.list_all_tools().await.expect("tools/list");
    assert!(
        tools.iter().any(|t| t.name == "query_graphql"),
        "legacy path must still serve tools, got {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn latest_on_toml_is_dual_era_on_mcp_and_proxy() {
    let dir = common::TempDir::new("vmcp-latest-on-toml");
    let cfg = write_config(dir.path(), Some(true), true);
    let gw = common::spawn_gateway(&cfg).await;

    let (status, init_2026) = initialize(&gw.mcp_url, LATEST).await;
    assert_eq!(status, 200, "initialize 2026: {init_2026}");
    assert_eq!(
        result_protocol_version(&init_2026),
        LATEST,
        "latest=true must accept 2026-07-28, got {init_2026}"
    );

    let (status, init_legacy) = initialize(&gw.mcp_url, LEGACY).await;
    assert_eq!(status, 200, "initialize legacy: {init_legacy}");
    assert_eq!(
        result_protocol_version(&init_legacy),
        LEGACY,
        "latest=true stays dual-era, got {init_legacy}"
    );

    let (status, disc) = discover(&gw.mcp_url, LATEST).await;
    assert_eq!(status, 200, "discover 2026: {disc}");
    let versions = supported_versions(&disc);
    assert!(
        versions.iter().any(|v| v == LATEST),
        "latest=true discover must list {LATEST}, got {versions:?}"
    );
    assert!(
        versions.iter().any(|v| v == LEGACY),
        "latest=true discover must still list {LEGACY}, got {versions:?}"
    );

    let (status, proxy_init) = initialize(&gw.proxy_url, LATEST).await;
    assert_eq!(status, 200, "proxy initialize 2026: {proxy_init}");
    assert_eq!(
        result_protocol_version(&proxy_init),
        LATEST,
        "/mcp-proxy must honor [mcp].latest, got {proxy_init}"
    );

    let (status, proxy_disc) = discover(&gw.proxy_url, LATEST).await;
    assert_eq!(status, 200, "proxy discover 2026: {proxy_disc}");
    let proxy_versions = supported_versions(&proxy_disc);
    assert!(
        proxy_versions.iter().any(|v| v == LATEST),
        "proxy discover must list {LATEST}, got {proxy_versions:?}"
    );
}

#[tokio::test]
async fn latest_on_via_env_accepts_2026() {
    let dir = common::TempDir::new("vmcp-latest-on-env");
    let cfg = write_config(dir.path(), Some(false), false);
    let gw = common::spawn_gateway_with_env(&cfg, true, &[("VMCP_MCP__LATEST", "true")]).await;

    let (status, init_2026) = initialize(&gw.mcp_url, LATEST).await;
    assert_eq!(status, 200, "env initialize 2026: {init_2026}");
    assert_eq!(
        result_protocol_version(&init_2026),
        LATEST,
        "VMCP_MCP__LATEST=true must accept 2026-07-28 even when TOML says false, got {init_2026}"
    );

    let (status, disc) = discover(&gw.mcp_url, LATEST).await;
    assert_eq!(status, 200, "env discover 2026: {disc}");
    let versions = supported_versions(&disc);
    assert!(
        versions.iter().any(|v| v == LATEST),
        "env latest must advertise {LATEST}, got {versions:?}"
    );
}

#[derive(Clone, Default)]
struct NullClient;

impl rmcp::ClientHandler for NullClient {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        rmcp::model::ClientInfo::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("mcp-latest-e2e", "0.0.0"),
        )
    }
}
