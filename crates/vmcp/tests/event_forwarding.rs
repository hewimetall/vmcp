//! Proves vmcp forwards events from internal MCP servers to the connected
//! client (the push side of the notification bus).
//!
//! A real `vmcp serve` gateway (Streamable-HTTP ingress) is booted with the
//! `mock_delay_upstream` fixture. A recording client connects over HTTP, then
//! calls the upstream via `query_graphql` with `emitEvent = true`; the mock
//! upstream responds and emits a `notifications/tools/list_changed`. vmcp's
//! `ForwardingClient` publishes it to the bus and the forwarder pushes it to
//! the client, whose `on_tool_list_changed` hook fires.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
use rmcp::service::{NotificationContext, RoleClient};
use rmcp::ClientHandler;
use serde_json::json;

#[derive(Clone)]
struct RecordingClient {
    got: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}
impl ClientHandler for RecordingClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("event-fwd-test", "0.0.0"),
        )
    }
    async fn on_tool_list_changed(&self, _ctx: NotificationContext<RoleClient>) {
        self.got.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[tokio::test]
async fn upstream_event_is_forwarded_to_client() {
    let dir = common::TempDir::new("vmcp-event-fwd");
    let mock = env!("CARGO_BIN_EXE_mock_delay_upstream");

    let registry = json!({
        "upstreams": [{
            "name": "mock",
            "command": mock,
            "args": [],
            "env": { "MOCK_LABEL": "mock" },
            "enabled": true,
            "sidecar_spec": "mock.json"
        }]
    });
    std::fs::write(
        dir.path().join("registry.json"),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();

    let specs = dir.path().join("specs");
    std::fs::create_dir_all(&specs).unwrap();
    std::fs::write(
        specs.join("mock.json"),
        serde_json::to_string_pretty(&json!({
            "server": "mock",
            "tools": [
                { "name": "delay_read", "read_only": true },
                { "name": "delay_write", "read_only": false }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let config_path = dir.path().join("vmcp.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
host = "127.0.0.1"
public_base_url = "http://127.0.0.1:8765"
registry_path = "{reg}"
lock_path     = "{lock}"
spec_dir      = "{spec}"
skills_dir    = "{skills}"

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
            reg = dir.path().join("registry.json").display(),
            lock = dir.path().join("tools.lock.json").display(),
            spec = specs.display(),
            skills = dir.path().join("skills").display(),
        ),
    )
    .unwrap();

    let gw = common::spawn_gateway(&config_path).await;

    let got = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let handler = RecordingClient {
        got: got.clone(),
        notify: notify.clone(),
    };
    let running = common::connect_client(handler, gw.mcp_url.clone()).await;

    // Call via GraphQL so the mock emits tools/list_changed after serving.
    let mut args = serde_json::Map::new();
    args.insert(
        "query".into(),
        json!("{ mock { delayRead(ms: 10, emitEvent: true) { json isError } } }"),
    );
    let _ = running
        .call_tool(CallToolRequestParams::new("query_graphql").with_arguments(args))
        .await
        .expect("call query_graphql");

    let waited = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if got.load(Ordering::SeqCst) {
                break;
            }
            notify.notified().await;
        }
    })
    .await;
    assert!(
        waited.is_ok() && got.load(Ordering::SeqCst),
        "client did not receive forwarded tools/list_changed"
    );

    running.cancel().await.ok();
}
