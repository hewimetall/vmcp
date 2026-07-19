//! Smoke test: spawn `vmcp serve` (Streamable-HTTP) and call `tools/list`
//! over the HTTP transport.

mod common;

use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::ClientHandler;

#[derive(Clone, Default)]
struct NullClient;

impl ClientHandler for NullClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("http-test", "0.0.0"),
        )
    }
}

fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
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
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        spec = dir.join("specs").display(),
        skills = dir.join("skills").display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

#[tokio::test]
async fn http_tools_list() {
    let dir = common::TempDir::new("vmcp-http-integ");
    let cfg = write_config(dir.path());

    let gw = common::spawn_gateway(&cfg).await;
    let client = common::connect_client(NullClient, gw.mcp_url.clone()).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    assert!(
        tools.iter().any(|t| t.name == "query_graphql"),
        "expected query_graphql tool, got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    client.cancel().await.ok();
}
