//! E2E for catalog isolation (G25 / ADR 0002), walked in the same order as
//! the operator docs:
//!
//! 1. [`docs/clients.md`] health + GraphQL discovery ladder
//!    (`prompts` → `servers` → `search`/`searchPrompts` → `__type`)
//! 2. [`docs/authentication.md`] static `pre-reg` scopes
//! 3. [`docs/upstreams.md`] `/mcp-proxy` `tools/list`
//! 4. [`docs/skills.md`] local YAML skill stays visible
//! 5. [`docs/adr/0002-per-caller-catalog-visibility.md`] three caller rows
//!
//! Hand-checked against a live `vmcp serve` before this test was added.

mod common;

use rmcp::model::*;
use rmcp::ClientHandler;
use vmcp_auth::static_tokens::{append_atomic, generate_entry};

const DEMO_ARGON2: &str = "$argon2id$v=19$m=19456,t=2,p=1$EKXF2yiUMT1injIS9ueldA$1Pra/zoGSKVIkZq1fCg0Hd2ceJuQn1H4k2lXeKUkMD8";

#[derive(Clone, Default)]
struct NullClient;

impl ClientHandler for NullClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("catalog-g25", "0.0.0"),
        )
    }
}

fn write_sidecar(dir: &std::path::Path, server: &str) {
    let spec = serde_json::json!({
        "server": server,
        "tools": [
            { "name": "delay_read", "read_only": true },
            { "name": "delay_write", "read_only": false }
        ]
    });
    std::fs::write(
        dir.join("specs").join(format!("{server}.json")),
        serde_json::to_vec_pretty(&spec).unwrap(),
    )
    .unwrap();
}

struct Tokens {
    dayana: String,
    admin: String,
    unscoped: String,
    admin_plus_whitelist: String,
}

fn setup_stand() -> (common::TempDir, std::path::PathBuf, Tokens) {
    let dir = common::TempDir::new("vmcp-catalog-g25");
    let root = dir.path();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::create_dir_all(root.join("specs")).unwrap();
    std::fs::create_dir_all(root.join("skills")).unwrap();

    // docs/skills.md — local YAML, no `server`; ADR 0002: always visible.
    std::fs::write(
        root.join("skills").join("search_docs.yaml"),
        r#"name: search_docs
description: Look up via GraphQL servers then search.
template: |
  Call query_graphql with:
  { servers { name } }
"#,
    )
    .unwrap();

    write_sidecar(root, "dayana");
    write_sidecar(root, "other");

    let exe = env!("CARGO_BIN_EXE_mock_delay_upstream");
    let registry = serde_json::json!({
        "upstreams": [
            {
                "name": "dayana",
                "description": "tenant dayana stand",
                "transport": "stdio",
                "command": exe,
                "env": { "MOCK_LABEL": "dayana" },
                "enabled": true,
                "sidecar_spec": "dayana.json"
            },
            {
                "name": "other",
                "description": "tenant other stand",
                "transport": "stdio",
                "command": exe,
                "env": { "MOCK_LABEL": "other" },
                "enabled": true,
                "sidecar_spec": "other.json"
            }
        ]
    });
    std::fs::write(
        root.join("registry.json"),
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    // docs/authentication.md — `vmcp pre-reg --scope …`
    let tokens_path = root.join("tokens.json");
    let dayana = generate_entry("dayana", Some("mcp:use upstream:dayana")).unwrap();
    let admin = generate_entry("ops", Some("mcp:admin")).unwrap();
    let unscoped = generate_entry("bot", Some("mcp:use")).unwrap();
    let admin_plus = generate_entry("ops2", Some("mcp:admin upstream:dayana")).unwrap();
    append_atomic(&tokens_path, &dayana).unwrap();
    append_atomic(&tokens_path, &admin).unwrap();
    append_atomic(&tokens_path, &unscoped).unwrap();
    append_atomic(&tokens_path, &admin_plus).unwrap();

    let cfg = root.join("vmcp.toml");
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
            reg = root.join("registry.json").display(),
            lock = root.join("tools.lock.json").display(),
            spec = root.join("specs").display(),
            skills = root.join("skills").display(),
            argon = DEMO_ARGON2,
            tokens = tokens_path.display(),
            clients = root.join("state").join("clients.db").display(),
        ),
    )
    .unwrap();

    (
        dir,
        cfg,
        Tokens {
            dayana: dayana.token,
            admin: admin.token,
            unscoped: unscoped.token,
            admin_plus_whitelist: admin_plus.token,
        },
    )
}

async fn connect(
    url: String,
    token: &str,
) -> rmcp::service::RunningService<rmcp::RoleClient, NullClient> {
    common::connect_client_with_token(NullClient, url, Some(token)).await
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

fn server_names(body: &serde_json::Value) -> Vec<String> {
    body["data"]["servers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect()
}

fn type_fields(body: &serde_json::Value) -> Vec<String> {
    body["data"]["__type"]["fields"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn docs_catalog_g25_ladder() {
    let (_dir, cfg, tokens) = setup_stand();
    let gw = common::spawn_gateway_auth(&cfg).await;

    // docs/clients.md — health is unauthenticated.
    let health = reqwest::get(format!("http://127.0.0.1:{}/health", gw.port))
        .await
        .expect("health");
    assert_eq!(health.text().await.unwrap().trim(), "ok");

    // --- ADR 0002 row 1: whitelist, no mcp:admin ---
    let tenant = connect(gw.mcp_url.clone(), &tokens.dayana).await;

    // docs/skills.md + clients.md step 1: local YAML prompt.
    let prompts = gql_json(&tenant, "{ prompts { name source server } }").await;
    let prompt_names: Vec<&str> = prompts["data"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(prompt_names, vec!["search_docs"]);
    assert_eq!(prompts["data"]["prompts"][0]["source"], "local");

    // clients.md step 2.
    let servers = gql_json(
        &tenant,
        "{ servers { name description toolCount readOnlyCount } }",
    )
    .await;
    assert_eq!(
        server_names(&servers),
        vec!["dayana".to_string()],
        "whitelist must not list other: {servers}"
    );

    // clients.md step 3.
    let search = gql_json(
        &tenant,
        r#"{ search(q: "delay") { server tool readOnly } }"#,
    )
    .await;
    let hits = search["data"]["search"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!hits.is_empty(), "{search}");
    assert!(
        hits.iter().all(|h| h["server"] == "dayana"),
        "search leaked other: {search}"
    );

    let sp = gql_json(&tenant, r#"{ searchPrompts(q: "search") { name source } }"#).await;
    assert_eq!(sp["data"]["searchPrompts"][0]["name"], "search_docs");

    // clients.md step 4 — scoped schema, not only resolver filters.
    let q_fields =
        type_fields(&gql_json(&tenant, r#"{ __type(name: "Query") { fields { name } } }"#).await);
    assert!(q_fields.contains(&"dayana".into()), "{q_fields:?}");
    assert!(!q_fields.contains(&"other".into()), "{q_fields:?}");
    let m_fields = type_fields(
        &gql_json(
            &tenant,
            r#"{ __type(name: "Mutation") { fields { name } } }"#,
        )
        .await,
    );
    assert_eq!(m_fields, vec!["dayana".to_string()], "{m_fields:?}");

    // docs/upstreams.md /mcp-proxy tools/list.
    let proxy = connect(
        format!("http://127.0.0.1:{}/mcp-proxy", gw.port),
        &tokens.dayana,
    )
    .await;
    let listed = proxy
        .list_tools(Default::default())
        .await
        .expect("proxy list");
    let proxy_names: Vec<String> = listed.tools.iter().map(|t| t.name.to_string()).collect();
    assert!(
        proxy_names.iter().any(|n| n.starts_with("dayana__")),
        "{proxy_names:?}"
    );
    assert!(
        proxy_names.iter().all(|n| !n.contains("other")),
        "{proxy_names:?}"
    );
    proxy.cancel().await.ok();

    // Call grant: own namespace works; foreign is unknown on scoped schema.
    let ok = gql_json(&tenant, "{ dayana { delayRead(ms: 1) { isError text } } }").await;
    assert_eq!(ok["data"]["dayana"]["delayRead"]["isError"], false, "{ok}");
    let hidden = gql_json(&tenant, "{ other { delayRead(ms: 1) { isError text } } }").await;
    let err = format!("{hidden}");
    assert!(
        err.contains("Unknown field") || err.contains("other"),
        "foreign namespace must not resolve: {hidden}"
    );
    tenant.cancel().await.ok();

    // --- ADR 0002 row 2: mcp:admin → full catalogue ---
    let admin = connect(gw.mcp_url.clone(), &tokens.admin).await;
    let mut admin_servers = server_names(&gql_json(&admin, "{ servers { name } }").await);
    admin_servers.sort();
    assert_eq!(
        admin_servers,
        vec!["dayana".to_string(), "other".to_string()]
    );
    let admin_q =
        type_fields(&gql_json(&admin, r#"{ __type(name: "Query") { fields { name } } }"#).await);
    assert!(admin_q.contains(&"dayana".into()) && admin_q.contains(&"other".into()));
    admin.cancel().await.ok();

    let admin_proxy = connect(
        format!("http://127.0.0.1:{}/mcp-proxy", gw.port),
        &tokens.admin,
    )
    .await;
    let admin_listed = admin_proxy
        .list_tools(Default::default())
        .await
        .expect("admin proxy");
    let admin_proxy_names: Vec<String> = admin_listed
        .tools
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(admin_proxy_names.iter().any(|n| n.starts_with("dayana__")));
    assert!(admin_proxy_names.iter().any(|n| n.starts_with("other__")));
    admin_proxy.cancel().await.ok();

    // --- ADR 0002 row 3: mcp:use without upstream:* → full catalogue ---
    let god = connect(gw.mcp_url.clone(), &tokens.unscoped).await;
    let mut god_servers = server_names(&gql_json(&god, "{ servers { name } }").await);
    god_servers.sort();
    assert_eq!(god_servers, vec!["dayana".to_string(), "other".to_string()]);
    god.cancel().await.ok();

    // Admin + whitelist: discovery full, calls still restricted (ADR 0002).
    let mixed = connect(gw.mcp_url.clone(), &tokens.admin_plus_whitelist).await;
    let mut mixed_servers = server_names(&gql_json(&mixed, "{ servers { name } }").await);
    mixed_servers.sort();
    assert_eq!(
        mixed_servers,
        vec!["dayana".to_string(), "other".to_string()],
        "mcp:admin keeps full catalog even with upstream:dayana"
    );
    let mixed_call = gql_json(&mixed, "{ other { delayRead(ms: 1) { isError text } } }").await;
    let mixed_text = format!("{mixed_call}");
    assert!(
        mixed_text.contains("forbidden")
            || mixed_text.contains("upstream:other")
            || mixed_call["data"]["other"]["delayRead"]["isError"] == true,
        "admin+whitelist must still block calls to other: {mixed_call}"
    );
    mixed.cancel().await.ok();
}
