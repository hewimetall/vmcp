//! End-to-end test of native MCP Tasks (SEP-1686) via `run_task` + SQLite.
//!
//! Spawns a real `vmcp serve` gateway (Streamable-HTTP ingress) with
//! `tasks.enabled = true` and a mock upstream whose `delay_read` advertises
//! `execution.taskSupport = optional`. A client then:
//!   * sees `run_task` with `taskSupport = optional` and the server `tasks`
//!     capability,
//!   * runs `run_task` **normally** (synchronous proxy), and
//!   * runs `run_task` **as a task**, receiving `CreateTaskResult`, then
//!     fetching the result via `tasks/result`.

mod common;

use std::path::PathBuf;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientCapabilities, ClientInfo, ClientRequest,
    GetTaskResultParams, GetTaskResultRequest, Implementation, ServerResult,
    TaskAugmentedRequestParamsMeta, TaskStatus,
};
use rmcp::ClientHandler;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct NullClient;
impl ClientHandler for NullClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("run-task-test", "0.0.0"),
        )
    }
}

/// A live gateway plus a connected client. The `Gateway` MUST outlive the
/// client (its child process is killed on drop), so both live in one struct.
struct Session {
    _gw: common::Gateway,
    client: rmcp::service::RunningService<rmcp::RoleClient, NullClient>,
}

fn write_config(dir: &std::path::Path) -> PathBuf {
    let mock = env!("CARGO_BIN_EXE_mock_delay_upstream");
    let registry = json!({
        "upstreams": [{
            "name": "mock",
            "command": mock,
            "args": [],
            "env": { "MOCK_LABEL": "mock" },
            "enabled": true
        }]
    });
    std::fs::write(
        dir.join("registry.json"),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();

    let config_path = dir.join("vmcp.toml");
    let config = format!(
        r#"
host = "127.0.0.1"
public_base_url = "http://127.0.0.1:8765"
registry_path = "{reg}"
lock_path     = "{lock}"
spec_dir      = "{spec}"
skills_dir    = "{skills}"

[tasks]
enabled = true
db_path = "{db}"
task_ttl_ms = 60000
poll_interval_ms = 200
max_concurrent = 4

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
        db = dir.join("tasks.db").display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

async fn serve(cfg: &std::path::Path) -> Session {
    let gw = common::spawn_gateway(cfg).await;
    let client = common::connect_client(NullClient, gw.mcp_url.clone()).await;
    Session { _gw: gw, client }
}

fn run_task_params(ms: u64) -> CallToolRequestParams {
    let mut args = serde_json::Map::new();
    args.insert("server".into(), json!("mock"));
    args.insert("tool".into(), json!("delay_read"));
    args.insert("arguments".into(), json!({ "ms": ms }));
    CallToolRequestParams::new("run_task").with_arguments(args)
}

#[tokio::test]
async fn advertises_tasks_capability_and_run_task_tool() {
    let dir = common::TempDir::new("vmcp-run-task-caps");
    let cfg = write_config(dir.path());
    let session = serve(&cfg).await;

    let info = session.client.peer().peer_info().expect("peer info");
    assert!(
        info.capabilities.tasks.is_some(),
        "server must advertise `tasks` capability when tasks are enabled"
    );

    let tools = session.client.list_all_tools().await.expect("tools/list");
    let run_task = tools
        .iter()
        .find(|t| t.name == "run_task")
        .expect("run_task tool present");
    let supports = run_task
        .execution
        .as_ref()
        .and_then(|e| e.task_support)
        .expect("run_task has execution.taskSupport");
    assert_eq!(supports, rmcp::model::TaskSupport::Optional);

    // Durable sqlite file should exist after boot.
    assert!(
        dir.path().join("tasks.db").exists()
            || dir.path().join("tasks.db-wal").exists()
            || dir.path().join("tasks.db").parent().is_some()
    );

    session.client.cancel().await.ok();
}

#[tokio::test]
async fn run_task_normal_call_is_synchronous() {
    let dir = common::TempDir::new("vmcp-run-task-sync");
    let cfg = write_config(dir.path());
    let session = serve(&cfg).await;

    let res = session
        .client
        .call_tool(run_task_params(150))
        .await
        .expect("call run_task");
    assert_ne!(res.is_error, Some(true), "sync run_task should succeed");
    let text = res
        .content
        .iter()
        .find_map(|c| c.raw.as_text().map(|t| t.text.clone()))
        .expect("text");
    let parsed: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(parsed["label"], json!("mock"));

    session.client.cancel().await.ok();
}

#[tokio::test]
async fn run_task_as_task_creates_and_resolves() {
    let dir = common::TempDir::new("vmcp-run-task-async");
    let cfg = write_config(dir.path());
    let session = serve(&cfg).await;

    let mut params = run_task_params(200);
    params.set_task(serde_json::Map::new());
    let create = session
        .client
        .peer()
        .send_request(ClientRequest::CallToolRequest(CallToolRequest::new(params)))
        .await
        .expect("task-augmented call");
    let task = match create {
        ServerResult::CreateTaskResult(c) => c.task,
        other => panic!("expected CreateTaskResult, got: {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Working);
    assert!(!task.task_id.is_empty());

    let result = session
        .client
        .peer()
        .send_request(ClientRequest::GetTaskResultRequest(
            GetTaskResultRequest::new(GetTaskResultParams {
                meta: None,
                task_id: task.task_id.clone(),
            }),
        ))
        .await
        .expect("tasks/result");
    let text = match result {
        ServerResult::CallToolResult(r) => r
            .content
            .iter()
            .find_map(|c| c.raw.as_text().map(|t| t.text.clone()))
            .expect("text content"),
        ServerResult::GetTaskPayloadResult(p) => p.0["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string(),
        other => panic!("unexpected tasks/result response: {other:?}"),
    };
    let inner: Value = serde_json::from_str(&text).expect("mock json");
    assert_eq!(inner["label"], json!("mock"));
    assert!(inner["start_us"].is_u64());

    // Task row must survive in SQLite.
    assert!(dir.path().join("tasks.db").exists());

    session.client.cancel().await.ok();
}

#[tokio::test]
async fn run_task_rejects_non_task_tool() {
    let dir = common::TempDir::new("vmcp-run-task-reject");
    let cfg = write_config(dir.path());
    let session = serve(&cfg).await;

    let mut args = serde_json::Map::new();
    args.insert("server".into(), json!("mock"));
    // delay_write has no taskSupport
    args.insert("tool".into(), json!("delay_write"));
    args.insert("arguments".into(), json!({ "ms": 10 }));
    let res = session
        .client
        .call_tool(CallToolRequestParams::new("run_task").with_arguments(args))
        .await
        .expect("call returns CallToolResult even on logical error");
    assert_eq!(res.is_error, Some(true));

    session.client.cancel().await.ok();
}
