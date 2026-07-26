//! Registry hot-reload via `POST /api/v1/upstreams/reload` (no process restart).

mod common;

use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use vmcp_auth::static_tokens::{append_atomic, generate_entry, SCOPE_ADMIN};

const DEMO_ARGON2: &str = "$argon2id$v=19$m=19456,t=2,p=1$EKXF2yiUMT1injIS9ueldA$1Pra/zoGSKVIkZq1fCg0Hd2ceJuQn1H4k2lXeKUkMD8";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Clone)]
struct Echo {
    #[allow(dead_code)]
    tool_router: ToolRouter<Echo>,
}

#[tool_router]
impl Echo {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo back the provided message.")]
    async fn echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let msg = args.msg.unwrap_or_else(|| "pong".to_string());
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }
}

#[tool_handler]
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("echo-http", "0.0.0"))
    }
}

async fn start_echo_upstream() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let allowed = vec![
        "127.0.0.1".to_string(),
        addr.to_string(),
        format!("localhost:{}", addr.port()),
    ];
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed);
    let service = StreamableHttpService::new(
        || Ok(Echo::new()),
        LocalSessionManager::default().into(),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr.port(), handle)
}

fn write_config(dir: &std::path::Path, tokens: &std::path::Path) -> std::path::PathBuf {
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
        tokens = tokens.display(),
        clients = dir.join("state").join("clients.db").display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

#[tokio::test]
async fn reload_adds_http_upstream_without_restart() {
    let (echo_port, _echo) = start_echo_upstream().await;
    let dir = common::TempDir::new("vmcp-registry-reload");
    let tokens = dir.path().join("tokens.json");
    let admin = generate_entry("operator", Some(SCOPE_ADMIN)).unwrap();
    append_atomic(&tokens, &admin).unwrap();
    let cfg = write_config(dir.path(), &tokens);
    let gw = common::spawn_gateway_auth(&cfg).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", admin.token);

    // Empty pool initially
    let resp = client
        .get(format!("http://127.0.0.1:{}/api/v1/upstreams", gw.port))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["upstreams"].as_array().unwrap().is_empty());

    // Atomic registry write adding echo HTTP upstream
    let reg = dir.path().join("registry.json");
    let reg_body = serde_json::json!({
        "upstreams": [{
            "name": "echo",
            "description": "hot-reloaded echo",
            "transport": "http",
            "url": format!("http://127.0.0.1:{echo_port}/mcp"),
            "enabled": true
        }]
    });
    let tmp = reg.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&reg_body).unwrap()).unwrap();
    std::fs::rename(&tmp, &reg).unwrap();

    // Force reload via API (also exercises the same path as the watcher).
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/v1/upstreams/reload",
            gw.port
        ))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let report: serde_json::Value = resp.json().await.unwrap();
    assert!(
        report["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "echo"),
        "report: {report}"
    );

    let resp = client
        .get(format!("http://127.0.0.1:{}/api/v1/upstreams", gw.port))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let ups = body["upstreams"].as_array().unwrap();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0]["name"], "echo");
    assert_eq!(ups[0]["connected"], true);
    assert!(ups[0]["tool_count"].as_u64().unwrap() >= 1);

    // Remove upstream and reload
    let empty = serde_json::json!({ "upstreams": [] });
    let tmp = reg.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&empty).unwrap()).unwrap();
    std::fs::rename(&tmp, &reg).unwrap();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/v1/upstreams/reload",
            gw.port
        ))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let report: serde_json::Value = resp.json().await.unwrap();
    assert!(report["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "echo"));
}
