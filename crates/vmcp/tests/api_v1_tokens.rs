//! Operator `/api/v1/tokens` e2e: Bearer + mcp:admin CRUD.

mod common;

use vmcp_auth::static_tokens::{append_atomic, generate_entry, SCOPE_ADMIN};

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
async fn api_v1_tokens_crud_and_scope_gate() {
    let dir = common::TempDir::new("vmcp-api-v1-tokens");
    let tokens = dir.path().join("tokens.json");
    let admin = generate_entry("operator", Some(SCOPE_ADMIN)).unwrap();
    let agent_pre = generate_entry("agent-pre", Some("mcp:use")).unwrap();
    append_atomic(&tokens, &admin).unwrap();
    append_atomic(&tokens, &agent_pre).unwrap();
    let cfg = write_auth_config(dir.path(), &tokens);
    let gw = common::spawn_gateway_auth(&cfg).await;
    let base = format!("http://127.0.0.1:{}/api/v1/tokens", gw.port);
    let client = reqwest::Client::new();

    // No bearer → 401
    let resp = client.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // mcp:use → 403
    let resp = client
        .get(&base)
        .header("Authorization", format!("Bearer {}", agent_pre.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // mcp:admin → list (redacted)
    let resp = client
        .get(&base)
        .header("Authorization", format!("Bearer {}", admin.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let list = body["tokens"].as_array().expect("tokens array");
    assert!(list.len() >= 2);
    assert!(list.iter().all(|t| t.get("token").is_none()));
    assert!(list.iter().any(|t| t["client_id"] == "operator"));

    // Create agent token
    let resp = client
        .post(&base)
        .header("Authorization", format!("Bearer {}", admin.token))
        .json(&serde_json::json!({"name":"agent-a","scope":"mcp:use"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let created: serde_json::Value = resp.json().await.unwrap();
    let new_token = created["token"].as_str().expect("token once");
    assert!(new_token.starts_with("vmcp_"));

    // Duplicate → 409
    let resp = client
        .post(&base)
        .header("Authorization", format!("Bearer {}", admin.token))
        .json(&serde_json::json!({"name":"agent-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);

    // New token works on /mcp
    let mcp =
        common::connect_client_with_token(NullClient, gw.mcp_url.clone(), Some(new_token)).await;
    let tools = mcp.list_all_tools().await.expect("tools/list");
    assert!(tools.iter().any(|t| t.name == "query_graphql"));
    mcp.cancel().await.ok();

    // Revoke agent-a
    let resp = client
        .delete(format!("{base}/agent-a"))
        .header("Authorization", format!("Bearer {}", admin.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    // Revoked token rejected
    let resp = reqwest::Client::new()
        .post(&gw.mcp_url)
        .header("Authorization", format!("Bearer {new_token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Cannot revoke last admin
    let resp = client
        .delete(format!("{base}/agent-pre"))
        .header("Authorization", format!("Bearer {}", admin.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = client
        .delete(format!("{base}/operator"))
        .header("Authorization", format!("Bearer {}", admin.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[derive(Clone, Default)]
struct NullClient;

impl rmcp::ClientHandler for NullClient {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        rmcp::model::ClientInfo::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("api-v1-test", "0.0.0"),
        )
    }
}
