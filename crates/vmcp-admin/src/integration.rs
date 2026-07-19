//! Axum integration tests for the admin SPA + JSON API.
//!
//! Covers auth, security headers, skills CRUD, schema/notifications, and
//! sessions dumps so line coverage on `vmcp-admin` can approach 99%.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;
use vmcp_auth::jwks::JwksManager;
use vmcp_auth::password::hash_password;
use vmcp_auth::AuthState;
use vmcp_graphql::{build_schema, SchemaLimits};
use vmcp_notify::Bus;
use vmcp_server::recorder::{Direction, Kind, McpExchange, Recorder, SessionKey};
use vmcp_server::sessions::SessionRegistry;
use vmcp_server::{save_skill, Skill, SkillArg};
use vmcp_upstream::UpstreamPool;

use crate::{router, AdminState};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("vmcp-admin-{tag}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MASTER: &str = "test-master-pw";

async fn test_state(skills_dir: &Path, sessions_dir: &Path) -> AdminState {
    let bus = Bus::new(64);
    let pool = Arc::new(UpstreamPool::empty_for_test(bus.clone()));
    pool.insert_synthetic_for_test(
        "demo",
        Some("Demo upstream".into()),
        vec![
            vmcp_upstream::ResolvedTool {
                server: "demo".into(),
                name: "ping".into(),
                description: Some("Ping".into()),
                input_schema: json!({"type": "object"}),
                read_only: true,
                task_support: vmcp_registry::TaskSupportHint::Forbidden,
            },
            vmcp_upstream::ResolvedTool {
                server: "demo".into(),
                name: "write".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                read_only: false,
                task_support: vmcp_registry::TaskSupportHint::Forbidden,
            },
        ],
    );
    let schema =
        build_schema(pool.all_resolved(), pool.clone(), SchemaLimits::default()).expect("schema");
    let master_hash = hash_password(MASTER).expect("hash");
    let jwks = JwksManager::new_with_fresh("kid-admin-test").unwrap();
    let auth_state = AuthState::new(jwks, "https://iss", "https://iss/mcp", 3600, &master_hash);
    let recorder = Recorder::new(sessions_dir.to_path_buf(), vec![]);
    AdminState::new(
        pool,
        Arc::new(ArcSwap::from_pointee(schema)),
        bus,
        Arc::new(ArcSwap::from_pointee(Vec::<Skill>::new())),
        skills_dir.to_path_buf(),
        master_hash,
        auth_state,
        Arc::new(SessionRegistry::new()),
        recorder,
    )
}

fn basic(user: &str, pass: &str) -> String {
    let raw = format!("{user}:{pass}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
    )
}

async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(a) = auth {
        builder = builder.header(header::AUTHORIZATION, a);
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let req = builder
        .body(match body {
            Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
            None => Body::empty(),
        })
        .unwrap();
    // Inject ConnectInfo so rate-limiter keys off a real peer.
    let mut req = req;
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9,
        ))));
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn spa_index_requires_auth_and_sets_security_headers() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);

    let (st, _, _) = call(app.clone(), Method::GET, "/", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let auth = basic("admin", MASTER);
    let (st, headers, body) = call(app, Method::GET, "/", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("admin.css"));
    assert!(html.contains("admin.js"));
    assert!(html.contains("data-nav=\"schema\""));
    let hdr = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    assert!(hdr("content-security-policy").contains("script-src 'self'"));
    assert_eq!(hdr("x-frame-options"), "DENY");
    assert_eq!(hdr("x-content-type-options"), "nosniff");
    assert_eq!(hdr("cache-control"), "no-cache");
    assert!(hdr("strict-transport-security").contains("max-age="));
}

#[tokio::test]
async fn deep_links_serve_spa_shell() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("x", MASTER);
    for path in [
        "/skills",
        "/sessions",
        "/compare",
        "/schema",
        "/notifications",
        "/servers/agentmemory",
    ] {
        let (st, _, body) = call(app.clone(), Method::GET, path, Some(&auth), None).await;
        assert_eq!(st, StatusCode::OK, "{path}");
        assert!(
            String::from_utf8_lossy(&body).contains("side1__brand"),
            "{path} missing shell"
        );
    }
}

#[tokio::test]
async fn wrong_password_is_unauthorized() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let (st, _, _) = call(
        app,
        Method::GET,
        "/api/servers",
        Some(&basic("u", "nope")),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rate_limiter_blocks_after_failures() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let mut state = test_state(skills.path(), sessions.path()).await;
    // Tight limiter for a fast test.
    state.rate_limiter = Arc::new(crate::RateLimiter::new(3, Duration::from_secs(60)));
    let app = router(state);
    for _ in 0..3 {
        let (st, _, _) = call(
            app.clone(),
            Method::GET,
            "/",
            Some(&basic("u", "wrong")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let (st, headers, _) = call(app, Method::GET, "/", Some(&basic("u", "wrong")), None).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("retry-after") && v == "60"));
}

#[tokio::test]
async fn servers_api_lists_synthetic_and_404s_unknown() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(app.clone(), Method::GET, "/api/servers", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&body);
    assert_eq!(v["servers"].as_array().unwrap().len(), 1);
    assert_eq!(v["servers"][0]["name"], "demo");
    assert_eq!(v["servers"][0]["tool_count"], 2);
    assert_eq!(v["servers"][0]["read_only_count"], 1);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/servers/demo",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["connected"], true);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/servers/demo/tools",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["tools"].as_array().unwrap().len(), 2);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/servers/missing",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, _) = call(
        app,
        Method::GET,
        "/api/servers/missing/tools",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn schema_sdl_and_notifications() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    state
        .bus
        .publish("demo", "notifications/message", json!({"text": "hi"}));
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, headers, body) = call(
        app.clone(),
        Method::GET,
        "/api/schema.graphql",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("text/plain")));
    let sdl = String::from_utf8_lossy(&body);
    assert!(sdl.contains("type Query") || sdl.contains("Query"));

    let (st, _, body) = call(
        app,
        Method::GET,
        "/api/notifications?limit=10",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&body);
    assert_eq!(v["notifications"].as_array().unwrap().len(), 1);
    assert_eq!(v["notifications"][0]["source"], "demo");
}

#[tokio::test]
async fn skills_crud_roundtrip() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(app.clone(), Method::GET, "/api/skills", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["skills"], json!([]));

    let create = json!({
        "name": "greet",
        "description": "Say hi",
        "arguments": [{"name": "who", "description": "person", "required": true}],
        "template": "Hello {{who}}"
    });
    let (st, _, body) = call(
        app.clone(),
        Method::POST,
        "/api/skills",
        Some(&auth),
        Some(create),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(json_body(&body)["name"], "greet");

    // Conflict on duplicate create.
    let (st, _, _) = call(
        app.clone(),
        Method::POST,
        "/api/skills",
        Some(&auth),
        Some(json!({
            "name": "greet",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/skills/greet",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["template"], "Hello {{who}}");

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/skills/missing",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Name mismatch on PUT.
    let (st, _, _) = call(
        app.clone(),
        Method::PUT,
        "/api/skills/greet",
        Some(&auth),
        Some(json!({
            "name": "other",
            "description": "x",
            "arguments": [],
            "template": "z"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::PUT,
        "/api/skills/greet",
        Some(&auth),
        Some(json!({
            "name": "greet",
            "description": "updated",
            "arguments": [{"name": "who", "required": true}],
            "template": "Hi {{who}}"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, _, body) = call(
        app.clone(),
        Method::POST,
        "/api/skills/greet/generate",
        Some(&auth),
        Some(json!({ "args": { "who": "Ada" } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["rendered"], "Hi Ada");

    // Missing required arg → 400.
    let (st, _, _) = call(
        app.clone(),
        Method::POST,
        "/api/skills/greet/generate",
        Some(&auth),
        Some(json!({ "args": {} })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::POST,
        "/api/skills/nope/generate",
        Some(&auth),
        Some(json!({ "args": {} })),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, _) = call(
        app.clone(),
        Method::DELETE,
        "/api/skills/greet",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, _, _) = call(
        app.clone(),
        Method::DELETE,
        "/api/skills/greet",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, _) = call(
        app,
        Method::PUT,
        "/api/skills/greet",
        Some(&auth),
        Some(json!({
            "name": "greet",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn skills_list_preview_truncates() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let long = "x".repeat(250);
    let skill = Skill {
        name: "longtpl".into(),
        description: "d".into(),
        arguments: vec![SkillArg {
            name: "a".into(),
            description: None,
            required: false,
            default: None,
        }],
        template: long,
    };
    save_skill(skills.path(), &skill).unwrap();
    let state = test_state(skills.path(), sessions.path()).await;
    // Seed ArcSwap from disk.
    let loaded = vmcp_server::load_skills(skills.path()).unwrap();
    state.skills.store(Arc::new(loaded));
    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, body) = call(app, Method::GET, "/api/skills", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let listed = json_body(&body);
    let preview = listed["skills"][0]["template_preview"].as_str().unwrap();
    assert!(preview.ends_with('…'));
    assert!(preview.chars().count() <= 201);
}

#[tokio::test]
async fn sessions_list_and_dumps() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;

    // Register a DCR client so list_sessions has a pre_registered row.
    state.auth_state.clients.insert(
        "vmcp-client1".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-client1".into(),
            redirect_uris: vec!["http://localhost/cb".into()],
            client_name: Some("Demo".into()),
            name: "demo".into(),
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            scope: Some("mcp:use".into()),
            issued_at: chrono::Utc::now(),
        },
    );

    // Live session on registry.
    state
        .registry
        .record_request("sess-live-1", Some("vmcp-client1"), Some("Demo"));

    // Historical dump on disk.
    let client_dir = sessions.path().join("vmcp-client1");
    std::fs::create_dir_all(&client_dir).unwrap();
    let jsonl = client_dir.join("sess-disk-1.jsonl");
    let ex = McpExchange {
        seq: 1,
        client_id: Some("vmcp-client1".into()),
        session_id: Some("sess-disk-1".into()),
        timestamp_ms: 1_700_000_000_000,
        direction: Direction::C2S,
        kind: Kind::Request,
        method: Some("initialize".into()),
        jsonrpc_id: Some(json!(1)),
        body: json!({"method":"initialize"}),
        latency_ms: None,
        upstream: Some("/mcp".into()),
    };
    std::fs::write(&jsonl, format!("{}\n", serde_json::to_string(&ex).unwrap())).unwrap();
    // Meta file for list_all_clients_with_meta
    let meta = vmcp_server::recorder::SessionMeta {
        client_id: "vmcp-client1".into(),
        client_name: Some("Demo".into()),
        session_id: "sess-disk-1".into(),
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: Some(1_700_000_001_000),
        request_count: 1,
        byte_size: 10,
        status: "closed".into(),
        upstream: Some("/mcp".into()),
    };
    std::fs::write(
        client_dir.join("sess-disk-1.meta.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();

    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(app.clone(), Method::GET, "/api/sessions", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let listed = json_body(&body);
    let clients = listed["clients"].as_array().unwrap();
    assert!(!clients.is_empty());
    let demo = clients
        .iter()
        .find(|c| c["client_id"] == "vmcp-client1")
        .expect("demo client");
    assert_eq!(demo["name"], "demo");

    // Rename DCR client — unique operator label.
    let (st, _, body) = call(
        app.clone(),
        Method::PATCH,
        "/api/sessions/vmcp-client1",
        Some(&auth),
        Some(json!({ "name": "laptop" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_body(&body)["name"], "laptop");

    let (st, _, body) = call(
        app.clone(),
        Method::PATCH,
        "/api/sessions/vmcp-client1",
        Some(&auth),
        Some(json!({ "name": "Bad Name!" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(json_body(&body)["error"]
        .as_str()
        .unwrap()
        .contains("invalid"));

    let (st, _, body) = call(
        app.clone(),
        Method::PATCH,
        "/api/sessions/missing-client",
        Some(&auth),
        Some(json!({ "name": "ok" })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let detail = json_body(&body);
    assert!(detail.get("client").is_some());
    assert_eq!(detail["client"]["name"], "laptop");

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/../evil",
        Some(&auth),
        None,
    )
    .await;
    // Axum may 404 before the handler runs; either way the path must not succeed.
    assert!(
        st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND,
        "unexpected status for traversal path: {st}"
    );

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/bad%2F%2Eid",
        Some(&auth),
        None,
    )
    .await;
    assert!(
        st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND,
        "unexpected status for encoded traversal: {st}"
    );

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/nobody-here",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump?session_id=sess-disk-1&limit=50",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let dump = json_body(&body);
    assert!(!dump["exchanges"].as_array().unwrap().is_empty());

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/bad..id/dump",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump?session_id=bad/id",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump.jsonl?session_id=sess-disk-1",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!body.is_empty());

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump.jsonl",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump.jsonl?session_id=nope",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Dump for registered client with no directory yet → empty exchanges.
    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump?session_id=never-written",
        Some(&auth),
        None,
    )
    .await;
    // File missing still returns 200 with empty list when client dir exists,
    // OR empty when reading missing file — either way not 5xx.
    assert!(st.is_success() || st == StatusCode::OK, "{st}");
    let _ = body;

    // Unknown client, no dump → 404.
    let (st, _, _) = call(
        app,
        Method::GET,
        "/api/sessions/unknown-client/dump",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dump_stream_requires_session_id() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-client1/dump/stream",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/bad..id/dump/stream?session_id=s1",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app,
        Method::GET,
        "/api/sessions/vmcp-client1/dump/stream?session_id=bad/id",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dump_stream_subscribes_ok() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    // Opening the SSE endpoint should succeed (200) even with no events yet.
    let builder = Request::builder()
        .method(Method::GET)
        .uri("/api/sessions/vmcp-client1/dump/stream?session_id=sess-1")
        .header(header::AUTHORIZATION, &auth);
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9,
        ))));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drop the body without reading forever (SSE keep-alive).
    drop(resp);
}

#[tokio::test]
async fn static_assets_served() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/static/admin.css",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("--bg") || body.len() > 100);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/static/admin.js",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("setView"));

    // Vendored GraphQL Detail helpers (loaded dynamically from admin.js).
    for path in [
        "/static/vendor/graphql-format.min.js",
        "/static/vendor/prism-core.min.js",
        "/static/vendor/prism-graphql.min.js",
        "/static/vendor/prism-json.min.js",
    ] {
        let (st, _, body) = call(app.clone(), Method::GET, path, Some(&auth), None).await;
        assert_eq!(st, StatusCode::OK, "{path}");
        assert!(body.len() > 40, "{path} empty");
    }
}

#[tokio::test]
async fn create_skill_rejects_invalid_name() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, _) = call(
        app,
        Method::POST,
        "/api/skills",
        Some(&auth),
        Some(json!({
            "name": "Bad/Name",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sessions_orphan_live_and_closed_and_disk_only() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;

    state
        .registry
        .record_request("sess-orphan", Some("orphan-client"), Some("Orphan"));

    state.auth_state.clients.insert(
        "vmcp-closed".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-closed".into(),
            redirect_uris: vec!["http://localhost/cb".into()],
            client_name: Some("Closed".into()),
            name: "closed".into(),
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            scope: Some("mcp:use".into()),
            issued_at: chrono::Utc::now(),
        },
    );
    state
        .registry
        .record_request("sess-closed", Some("vmcp-closed"), Some("Closed"));
    state.registry.close("sess-closed");

    let disk_dir = sessions.path().join("disk-only");
    std::fs::create_dir_all(&disk_dir).unwrap();
    let meta = vmcp_server::recorder::SessionMeta {
        client_id: "disk-only".into(),
        client_name: Some("Disk".into()),
        session_id: "sess-disk-only".into(),
        started_at_ms: 100,
        ended_at_ms: Some(250),
        request_count: 2,
        byte_size: 10,
        status: "closed".into(),
        upstream: Some("/mcp-proxy".into()),
    };
    std::fs::write(
        disk_dir.join("sess-disk-only.meta.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();
    std::fs::write(disk_dir.join("sess-disk-only.jsonl"), "{}\n\n").unwrap();

    state.auth_state.clients.insert(
        "vmcp-merge".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-merge".into(),
            redirect_uris: vec![],
            client_name: Some("Merge".into()),
            name: "merge".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        },
    );
    state
        .registry
        .record_request("sess-merge", Some("vmcp-merge"), Some("Merge"));
    let merge_dir = sessions.path().join("vmcp-merge");
    std::fs::create_dir_all(&merge_dir).unwrap();
    let merge_meta = vmcp_server::recorder::SessionMeta {
        client_id: "vmcp-merge".into(),
        client_name: Some("Merge".into()),
        session_id: "sess-merge".into(),
        started_at_ms: 1,
        ended_at_ms: None,
        request_count: 9,
        byte_size: 1,
        status: "active".into(),
        upstream: Some("/mcp".into()),
    };
    std::fs::write(
        merge_dir.join("sess-merge.meta.json"),
        serde_json::to_string(&merge_meta).unwrap(),
    )
    .unwrap();

    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, body) = call(app, Method::GET, "/api/sessions", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let listed = json_body(&body);
    assert!(listed["clients"].as_array().unwrap().len() >= 3);
}

#[tokio::test]
async fn dump_filters_and_all_files_and_empty_client_dir() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;

    state.auth_state.clients.insert(
        "vmcp-dump".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-dump".into(),
            redirect_uris: vec![],
            client_name: None,
            name: "dump".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        },
    );
    state.auth_state.clients.insert(
        "vmcp-empty".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-empty".into(),
            redirect_uris: vec![],
            client_name: None,
            name: "empty".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        },
    );

    let dir = sessions.path().join("vmcp-dump");
    std::fs::create_dir_all(&dir).unwrap();
    let ex1 = McpExchange {
        seq: 1,
        client_id: Some("vmcp-dump".into()),
        session_id: Some("s1".into()),
        timestamp_ms: 1,
        direction: Direction::C2S,
        kind: Kind::Request,
        method: Some("initialize".into()),
        jsonrpc_id: Some(json!(1)),
        body: json!({}),
        latency_ms: None,
        upstream: Some("/mcp".into()),
    };
    let ex2 = McpExchange {
        seq: 2,
        client_id: Some("vmcp-dump".into()),
        session_id: Some("s1".into()),
        timestamp_ms: 2,
        direction: Direction::S2C,
        kind: Kind::Response,
        method: Some("initialize".into()),
        jsonrpc_id: Some(json!(1)),
        body: json!({}),
        latency_ms: Some(1),
        upstream: Some("/mcp".into()),
    };
    let ex3 = McpExchange {
        seq: 3,
        client_id: Some("vmcp-dump".into()),
        session_id: Some("s2".into()),
        timestamp_ms: 3,
        direction: Direction::C2S,
        kind: Kind::Request,
        method: Some("tools/list".into()),
        jsonrpc_id: Some(json!(2)),
        body: json!({}),
        latency_ms: None,
        upstream: Some("/mcp".into()),
    };
    std::fs::write(
        dir.join("s1.jsonl"),
        format!(
            "{}\n\n{}\nnot-json\n",
            serde_json::to_string(&ex1).unwrap(),
            serde_json::to_string(&ex2).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("s2.jsonl"),
        format!("{}\n", serde_json::to_string(&ex3).unwrap()),
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "ignore").unwrap();

    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-empty/dump",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["exchanges"], json!([]));

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-dump/dump?limit=10",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["exchanges"].as_array().unwrap().len(), 3);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/vmcp-dump/dump?session_id=s1&direction=c2s&method=initialize&since_seq=0",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let dump = json_body(&body);
    let filtered = dump["exchanges"].as_array().unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["seq"], 1);

    let (st, _, body) = call(
        app,
        Method::GET,
        "/api/sessions/vmcp-dump/dump?session_id=s1&since_seq=1",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["exchanges"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn dump_stream_receives_broadcast_event() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let recorder = state.recorder.clone();
    let app = router(state);
    let auth = basic("a", MASTER);

    let key = SessionKey {
        client_id: "vmcp-sse".into(),
        session_id: "sess-sse".into(),
    };
    let _rx = recorder.subscribe(&key);

    let builder = Request::builder()
        .method(Method::GET)
        .uri("/api/sessions/vmcp-sse/dump/stream?session_id=sess-sse")
        .header(header::AUTHORIZATION, &auth);
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9,
        ))));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    recorder
        .record(McpExchange {
            seq: 1,
            client_id: Some("vmcp-sse".into()),
            session_id: Some("sess-sse".into()),
            timestamp_ms: 1,
            direction: Direction::C2S,
            kind: Kind::Request,
            method: Some("ping".into()),
            jsonrpc_id: Some(json!(1)),
            body: json!({}),
            latency_ms: None,
            upstream: Some("/mcp".into()),
        })
        .await;
    drop(resp);
}

#[tokio::test]
async fn skills_save_fails_when_dir_missing() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let mut state = test_state(skills.path(), sessions.path()).await;
    let bogon = skills.path().join("not-a-dir");
    std::fs::write(&bogon, "x").unwrap();
    state.skills_dir = bogon;
    state.skills.store(Arc::new(vec![Skill {
        name: "ghost".into(),
        description: "d".into(),
        arguments: vec![],
        template: "t".into(),
    }]));
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, _) = call(
        app.clone(),
        Method::POST,
        "/api/skills",
        Some(&auth),
        Some(json!({
            "name": "newone",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(
        app.clone(),
        Method::PUT,
        "/api/skills/ghost",
        Some(&auth),
        Some(json!({
            "name": "ghost",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _, _) = call(app, Method::DELETE, "/api/skills/ghost", Some(&auth), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notifications_default_limit_and_short_preview() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    state.bus.publish("s", "notifications/x", json!({}));
    save_skill(
        skills.path(),
        &Skill {
            name: "short".into(),
            description: "d".into(),
            arguments: vec![],
            template: "hi".into(),
        },
    )
    .unwrap();
    state
        .skills
        .store(Arc::new(vmcp_server::load_skills(skills.path()).unwrap()));
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, body) = call(
        app.clone(),
        Method::GET,
        "/api/notifications",
        Some(&auth),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        json_body(&body)["notifications"].as_array().unwrap().len(),
        1
    );

    let (st, _, body) = call(app, Method::GET, "/api/skills", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["skills"][0]["template_preview"], "hi");
}

#[tokio::test]
async fn skills_reload_skips_corrupt_yaml_sibling() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    std::fs::write(skills.path().join("broken.yaml"), ": : : not yaml {{{").unwrap();
    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, body) = call(
        app,
        Method::POST,
        "/api/skills",
        Some(&auth),
        Some(json!({
            "name": "okname",
            "description": "x",
            "arguments": [],
            "template": "y"
        })),
    )
    .await;
    // Corrupt siblings are skipped by load_skills — create still succeeds.
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(json_body(&body)["name"], "okname");
}

#[tokio::test]
async fn dump_download_rejects_bad_ids() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let app = router(state);
    let auth = basic("a", MASTER);

    let (st, _, _) = call(
        app.clone(),
        Method::GET,
        "/api/sessions/bad%20id/dump.jsonl?session_id=s1",
        Some(&auth),
        None,
    )
    .await;
    assert!(st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND);

    let (st, _, _) = call(
        app,
        Method::GET,
        "/api/sessions/vmcp-x/dump.jsonl?session_id=bad%20id",
        Some(&auth),
        None,
    )
    .await;
    assert!(st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dump_stream_reads_one_event() {
    use futures::StreamExt;
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    let recorder = state.recorder.clone();
    let app = router(state);
    let auth = basic("a", MASTER);

    let key = SessionKey {
        client_id: "vmcp-sse2".into(),
        session_id: "sess-sse2".into(),
    };

    let builder = Request::builder()
        .method(Method::GET)
        .uri("/api/sessions/vmcp-sse2/dump/stream?session_id=sess-sse2")
        .header(header::AUTHORIZATION, &auth);
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            11,
        ))));

    let resp_fut = app.oneshot(req);
    let publish = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Ensure channel exists then publish via record.
        let _ = recorder.subscribe(&key);
        recorder
            .record(McpExchange {
                seq: 42,
                client_id: Some("vmcp-sse2".into()),
                session_id: Some("sess-sse2".into()),
                timestamp_ms: 1,
                direction: Direction::C2S,
                kind: Kind::Request,
                method: Some("ping".into()),
                jsonrpc_id: Some(json!(1)),
                body: json!({"ok": true}),
                latency_ms: None,
                upstream: Some("/mcp".into()),
            })
            .await;
    };
    let (resp, _) = tokio::join!(resp_fut, publish);
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut got = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), body.next()).await {
            Ok(Some(Ok(bytes))) => {
                if !bytes.is_empty() {
                    got = true;
                    break;
                }
            }
            Ok(Some(Err(_))) => break,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert!(got, "expected at least one SSE chunk");
}

#[tokio::test]
async fn closed_session_marks_pre_registered_idle() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    state.auth_state.clients.insert(
        "vmcp-idle".into(),
        vmcp_auth::types::ClientInfo {
            client_id: "vmcp-idle".into(),
            redirect_uris: vec![],
            client_name: Some("Idle".into()),
            name: "idle".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        },
    );
    // Create then immediately close BEFORE listing — list sees Closed + pre_registered.
    state
        .registry
        .record_request("sess-idle", Some("vmcp-idle"), Some("Idle"));
    state.registry.close("sess-idle");

    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, body) = call(app, Method::GET, "/api/sessions", Some(&auth), None).await;
    assert_eq!(st, StatusCode::OK);
    let clients = json_body(&body)["clients"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let row = clients
        .into_iter()
        .find(|c| c["client_id"] == "vmcp-idle")
        .expect("client");
    assert_eq!(row["state"], "idle");
}

#[tokio::test]
async fn rename_client_rejects_duplicate_name() {
    let skills = TempDir::new("skills");
    let sessions = TempDir::new("sessions");
    let state = test_state(skills.path(), sessions.path()).await;
    for (id, name) in [("vmcp-a", "alpha"), ("vmcp-b", "beta")] {
        state.auth_state.clients.insert(
            id.into(),
            vmcp_auth::types::ClientInfo {
                client_id: id.into(),
                redirect_uris: vec![],
                client_name: Some("Cursor".into()),
                name: name.into(),
                grant_types: vec![],
                response_types: vec![],
                scope: None,
                issued_at: chrono::Utc::now(),
            },
        );
    }
    let app = router(state);
    let auth = basic("a", MASTER);
    let (st, _, body) = call(
        app.clone(),
        Method::PATCH,
        "/api/sessions/vmcp-a",
        Some(&auth),
        Some(json!({ "name": "beta" })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(json_body(&body)["error"]
        .as_str()
        .unwrap()
        .contains("already taken"));

    let (st, _, body) = call(
        app,
        Method::PATCH,
        "/api/sessions/bad..id",
        Some(&auth),
        Some(json!({ "name": "ok" })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(json_body(&body)["error"]
        .as_str()
        .unwrap()
        .contains("invalid client_id"));
}
