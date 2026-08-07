//! Proves the native Streamable-HTTP upstream transport (B2): vmcp's
//! `UpstreamPool` can connect to a remote MCP server over HTTP instead of
//! spawning a stdio child.
//!
//! An in-process rmcp `StreamableHttpService` hosts a trivial `echo` server on
//! an ephemeral port; the pool is then booted with a single `transport = "http"`
//! upstream pointed at it and we verify discovery + a tool call round-trip.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::json;
use vmcp_notify::Bus;
use vmcp_registry::{Registry, UpstreamSpec, UpstreamTransport};
use vmcp_upstream::UpstreamPool;

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

#[tokio::test]
async fn http_upstream_discovers_and_calls() {
    // 1. Bind an ephemeral port and host the echo server over Streamable HTTP.
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
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the listener a moment to start serving.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Boot the pool with a single HTTP upstream pointed at the echo server.
    let url = format!("http://{addr}/mcp");
    let registry = Registry {
        upstreams: vec![UpstreamSpec {
            name: "echo".into(),
            description: Some("in-process http echo".into()),
            transport: UpstreamTransport::Http,
            url: Some(url),
            bearer: None,
            command: String::new(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
            forward_identity: false,
        }],
    };
    let bus = Bus::new(1024);
    let (pool, failures) = UpstreamPool::spawn_all(
        &registry,
        bus,
        None,
        Duration::from_secs(20),
        Duration::from_secs(20),
    )
    .await;
    assert!(
        failures.is_empty(),
        "http upstream spawn failures: {failures:?}"
    );
    let pool = Arc::new(pool);

    // 3. Discovery: the echo tool is visible through the HTTP transport.
    let resolved = pool.resolved("echo").expect("echo upstream resolved");
    assert!(
        resolved.iter().any(|t| t.name == "echo"),
        "expected `echo` tool, got: {:?}",
        resolved.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // 4. Round-trip a tool call over HTTP.
    let res = pool
        .call("echo", "echo", json!({ "msg": "hello-over-http" }), None)
        .await
        .expect("call echo over http");
    let text = res
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    assert_eq!(text, "hello-over-http");

    pool.shutdown().await;
    server.abort();
}

/// Defect 2: HTTP upstream tool calls carry caller subject/groups while
/// registry `bearer` stays on `Authorization`.
#[tokio::test]
async fn http_upstream_forwards_caller_identity_headers() {
    use axum::body::Body;
    use axum::extract::Request;
    use axum::middleware::{from_fn, Next};
    use axum::response::Response;
    use std::sync::Mutex;
    use vmcp_upstream::{CallerIdentity, HEADER_GROUPS, HEADER_SUBJECT};

    let seen: Arc<Mutex<Vec<(Option<String>, Option<String>, Option<String>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let seen_mw = seen.clone();

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

    async fn capture(
        seen: Arc<Mutex<Vec<(Option<String>, Option<String>, Option<String>)>>>,
        req: Request<Body>,
        next: Next,
    ) -> Response {
        let subject = req
            .headers()
            .get(HEADER_SUBJECT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let groups = req
            .headers()
            .get(HEADER_GROUPS)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let auth = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        seen.lock().unwrap().push((subject, groups, auth));
        next.run(req).await
    }

    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(from_fn(move |req, next| {
            let seen = seen_mw.clone();
            capture(seen, req, next)
        }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = format!("http://{addr}/mcp");
    let registry = Registry {
        upstreams: vec![UpstreamSpec {
            name: "echo".into(),
            description: None,
            transport: UpstreamTransport::Http,
            url: Some(url),
            bearer: Some("svc-token".into()),
            command: String::new(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            sidecar_spec: None,
            enabled: true,
            forward_identity: true,
        }],
    };
    let bus = Bus::new(1024);
    let (pool, failures) = UpstreamPool::spawn_all(
        &registry,
        bus,
        None,
        Duration::from_secs(20),
        Duration::from_secs(20),
    )
    .await;
    assert!(failures.is_empty(), "{failures:?}");
    let pool = Arc::new(pool);

    let caller = CallerIdentity::new(
        "alice",
        vec!["stand-foo".into(), "mcp-users".into()],
        "alice",
        "mcp:use",
    );
    let res = pool
        .call(
            "echo",
            "echo",
            json!({ "msg": "with-identity" }),
            Some(&caller),
        )
        .await
        .expect("call with identity");
    assert!(!res.is_error.unwrap_or(false));

    let snapshots = seen.lock().unwrap().clone();
    assert!(
        snapshots.iter().any(|(s, g, a)| {
            s.as_deref() == Some("alice")
                && g.as_deref() == Some("stand-foo,mcp-users")
                && a.as_deref() == Some("Bearer svc-token")
        }),
        "expected X-Vmcp-* + service Bearer on a tools/call; saw {snapshots:?}"
    );

    pool.shutdown().await;
    server.abort();
}
