//! Scope enforcement: `upstream:` whitelist blocks other GraphQL namespaces.

mod common;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use vmcp_auth::static_tokens::{append_atomic, generate_entry};

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
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }
}

#[tool_handler]
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("echo-http", "0.0.0"))
    }
}

async fn start_echo(name: &str) -> (u16, tokio::task::JoinHandle<()>) {
    let _ = name;
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
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    (addr.port(), handle)
}

#[tokio::test]
async fn upstream_scope_blocks_other_server() {
    let (port_a, _ha) = start_echo("alpha").await;
    let (port_b, _hb) = start_echo("beta").await;

    let dir = common::TempDir::new("vmcp-scope-enforce");
    let tokens = dir.path().join("tokens.json");
    let scoped = generate_entry("agent", Some("mcp:use upstream:alpha")).unwrap();
    let admin = generate_entry("ops", Some("mcp:admin")).unwrap();
    append_atomic(&tokens, &scoped).unwrap();
    append_atomic(&tokens, &admin).unwrap();

    let registry = serde_json::json!({
        "upstreams": [
            {
                "name": "alpha",
                "transport": "http",
                "url": format!("http://127.0.0.1:{port_a}/mcp"),
                "enabled": true
            },
            {
                "name": "beta",
                "transport": "http",
                "url": format!("http://127.0.0.1:{port_b}/mcp"),
                "enabled": true
            }
        ]
    });
    std::fs::write(
        dir.path().join("registry.json"),
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::create_dir_all(dir.path().join("specs")).unwrap();
    std::fs::create_dir_all(dir.path().join("skills")).unwrap();

    let cfg = dir.path().join("vmcp.toml");
    std::fs::write(
        &cfg,
        format!(
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

[proxy]
enabled = true
mcp_path = "/mcp-proxy"
"#,
            reg = dir.path().join("registry.json").display(),
            lock = dir.path().join("tools.lock.json").display(),
            spec = dir.path().join("specs").display(),
            skills = dir.path().join("skills").display(),
            argon = DEMO_ARGON2,
            tokens = tokens.display(),
            clients = dir.path().join("state").join("clients.db").display(),
        ),
    )
    .unwrap();

    let gw = common::spawn_gateway_auth(&cfg).await;
    let client =
        common::connect_client_with_token(NullClient, gw.mcp_url.clone(), Some(&scoped.token))
            .await;

    // Echo has no readOnlyHint → Mutation bucket.
    let gql = |server: &str, msg: &str| {
        format!("mutation {{ {server} {{ echo(msg: \"{msg}\") {{ text isError }} }} }}")
    };

    // Allowed namespace
    let ok = client
        .call_tool(
            CallToolRequestParams::new("query_graphql").with_arguments(
                serde_json::json!({ "query": gql("alpha", "hi") })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("alpha call");
    let ok_text = format!("{ok:?}");
    assert!(
        ok_text.contains("hi") && !ok_text.contains("forbidden"),
        "alpha should be allowed: {ok_text}"
    );

    // Denied namespace
    let denied = client
        .call_tool(
            CallToolRequestParams::new("query_graphql").with_arguments(
                serde_json::json!({ "query": gql("beta", "nope") })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("beta call returns tool result");
    let denied_text = format!("{denied:?}");
    assert!(
        denied_text.contains("forbidden")
            || denied_text.contains("upstream:beta")
            || denied_text.contains("Unknown field"),
        "beta should be forbidden: {denied_text}"
    );

    let catalog = gql_json(
        &client,
        "{ servers { name } search(q: \"echo\") { server tool } }",
    )
    .await;
    let servers = catalog["data"]["servers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let server_names: Vec<&str> = servers.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(
        server_names,
        vec!["alpha"],
        "whitelist catalog must not list beta: {catalog}"
    );
    let hits = catalog["data"]["search"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        hits.iter().all(|h| h["server"] == "alpha"),
        "search leaked foreign upstream: {catalog}"
    );

    let intro = gql_json(
        &client,
        r#"{ __type(name: "Mutation") { fields { name } } }"#,
    )
    .await;
    let intro_fields = intro["data"]["__type"]["fields"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut_fields: Vec<&str> = intro_fields
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(
        mut_fields.contains(&"alpha"),
        "alpha mutation namespace missing: {intro}"
    );
    assert!(
        !mut_fields.contains(&"beta"),
        "beta mutation namespace leaked via introspection: {intro}"
    );

    let proxy = common::connect_client_with_token(
        NullClient,
        format!("http://127.0.0.1:{}/mcp-proxy", gw.port),
        Some(&scoped.token),
    )
    .await;
    let listed = proxy
        .list_tools(Default::default())
        .await
        .expect("proxy list");
    let names: Vec<String> = listed.tools.iter().map(|t| t.name.to_string()).collect();
    assert!(
        names.iter().any(|n| n.starts_with("alpha__")),
        "proxy list missing alpha: {names:?}"
    );
    assert!(
        names.iter().all(|n| !n.starts_with("beta__")),
        "proxy list leaked beta: {names:?}"
    );
    proxy.cancel().await.ok();

    let admin_client =
        common::connect_client_with_token(NullClient, gw.mcp_url.clone(), Some(&admin.token)).await;
    let admin_catalog = gql_json(&admin_client, "{ servers { name } }").await;
    let admin_servers = admin_catalog["data"]["servers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut admin_names: Vec<&str> = admin_servers
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    admin_names.sort();
    assert_eq!(
        admin_names,
        vec!["alpha", "beta"],
        "mcp:admin must see full catalog: {admin_catalog}"
    );
    admin_client.cancel().await.ok();

    client.cancel().await.ok();
}

async fn gql_json(
    client: &rmcp::service::RunningService<rmcp::RoleClient, NullClient>,
    query: &str,
) -> serde_json::Value {
    let result = client
        .call_tool(
            CallToolRequestParams::new("query_graphql").with_arguments(
                serde_json::json!({ "query": query })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("query_graphql");
    for c in &result.content {
        if let ContentBlock::Text(t) = c {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.text) {
                return v;
            }
        }
    }
    panic!("query_graphql had no JSON text: {result:?}");
}

#[derive(Clone, Default)]
struct NullClient;

impl rmcp::ClientHandler for NullClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("scope-test", "0.0.0"),
        )
    }
}
