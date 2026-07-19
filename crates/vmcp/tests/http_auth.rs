//! Auth e2e: Streamable-HTTP `/mcp` with `auth.enabled = true`.
//!
//! Missing Bearer → 401. Static pre-reg token → tools/list OK.

mod common;

use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::ClientHandler;
use vmcp_auth::static_tokens::{append_atomic, generate_entry};

#[derive(Clone, Default)]
struct NullClient;

impl ClientHandler for NullClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("http-auth-test", "0.0.0"),
        )
    }
}

/// Valid demo argon2id hash from `vmcp.toml` (password `demo-master`).
const DEMO_ARGON2: &str = "$argon2id$v=19$m=19456,t=2,p=1$EKXF2yiUMT1injIS9ueldA$1Pra/zoGSKVIkZq1fCg0Hd2ceJuQn1H4k2lXeKUkMD8";

fn write_auth_config(dir: &std::path::Path, tokens_file: &std::path::Path) -> std::path::PathBuf {
    std::fs::write(dir.join("registry.json"), br#"{"upstreams":[]}"#).unwrap();
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::create_dir_all(dir.join("specs")).unwrap();
    std::fs::create_dir_all(dir.join("skills")).unwrap();

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
enabled = true
master_password_argon2 = "{argon}"
jwt_kid = "test"
jwks_rotate_secs = 86400
token_ttl_secs = 3600
tokens_file = "{tokens}"
clients_db_path = "{clients}"
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
        argon = DEMO_ARGON2,
        tokens = tokens_file.display(),
        clients = dir.join("state").join("clients.db").display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

#[tokio::test]
async fn missing_bearer_is_unauthorized() {
    let dir = common::TempDir::new("vmcp-http-auth-401");
    let tokens = dir.path().join("tokens.json");
    let entry = generate_entry("ci", Some("mcp:use")).unwrap();
    append_atomic(&tokens, &entry).unwrap();
    let cfg = write_auth_config(dir.path(), &tokens);

    let gw = common::spawn_gateway_auth(&cfg).await;

    let resp = reqwest::Client::new()
        .post(&gw.mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#)
        .send()
        .await
        .expect("POST /mcp");

    let status = resp.status();
    let www_authenticate = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "body: {:?}",
        resp.text().await
    );
    let www_authenticate = www_authenticate.expect("WWW-Authenticate header");
    assert!(
        www_authenticate.contains(r#"error="missing_bearer""#),
        "unexpected WWW-Authenticate header: {www_authenticate}"
    );
}

#[tokio::test]
async fn static_token_tools_list_ok() {
    let dir = common::TempDir::new("vmcp-http-auth-ok");
    let tokens = dir.path().join("tokens.json");
    let entry = generate_entry("ci", Some("mcp:use")).unwrap();
    let token = entry.token.clone();
    append_atomic(&tokens, &entry).unwrap();
    let cfg = write_auth_config(dir.path(), &tokens);

    let gw = common::spawn_gateway_auth(&cfg).await;
    let client =
        common::connect_client_with_token(NullClient, gw.mcp_url.clone(), Some(&token)).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    assert!(
        tools.iter().any(|t| t.name == "query_graphql"),
        "expected query_graphql tool, got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    client.cancel().await.ok();
}
