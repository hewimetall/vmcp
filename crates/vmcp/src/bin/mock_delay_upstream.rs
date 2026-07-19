//! Test fixture — a tiny stdio MCP upstream used by the aggregation
//! integration tests (`crates/vmcp/tests/aggregation.rs`).
//!
//! It exposes two tools that do nothing but sleep for `ms` milliseconds and
//! then report the wall-clock window (`start_us`/`end_us`, microseconds since
//! the Unix epoch) during which the call was being served:
//!
//! * `delay_read`  — bucketed into `Query`    by the test sidecar (readOnly).
//! * `delay_write` — bucketed into `Mutation` by the test sidecar (write).
//!
//! Two of these processes are spawned as independent upstreams (`alpha`,
//! `beta`). Because the windows are wall-clock and the machine clock is shared
//! across processes, the test can prove that vmcp's *read* fan-out overlaps in
//! time (parallel aggregation) while its *write* fan-out does not (sequential
//! aggregation) — purely from the reported timestamps, no internal hooks.
//!
//! `MOCK_LABEL` (env) tags the JSON so a test can tell `alpha` from `beta`.
//! Not shipped in release builds is not a goal here: it is a dev/test helper
//! that lives next to the binary it exercises.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::json;

fn now_us() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DelayArgs {
    /// Milliseconds to sleep before returning. Defaults to 200.
    #[serde(default)]
    ms: Option<u64>,
    /// When true, emit a `notifications/tools/list_changed` to the client
    /// (vmcp) after serving — used to exercise event forwarding.
    #[serde(default)]
    emit_event: Option<bool>,
}

#[derive(Clone)]
struct MockDelay {
    label: String,
    // Held so the `#[tool_router]`-generated router stays alive; read via the
    // `#[tool_handler]` impl, not directly.
    #[allow(dead_code)]
    tool_router: ToolRouter<MockDelay>,
}

#[tool_router]
impl MockDelay {
    fn new(label: String) -> Self {
        Self {
            label,
            tool_router: Self::tool_router(),
        }
    }

    async fn serve_delay(&self, args: DelayArgs) -> CallToolResult {
        let ms = args.ms.unwrap_or(200);
        let start_us = now_us();
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let end_us = now_us();
        let body = json!({
            "label": self.label,
            "ms": ms,
            "start_us": start_us as u64,
            "end_us": end_us as u64,
        });
        CallToolResult::success(vec![Content::text(body.to_string())])
    }

    /// Read-only delay probe (sidecar marks it readOnly → GraphQL `Query`).
    /// Marked task-capable so integration tests can exercise `run_task`.
    #[tool(
        description = "Sleep `ms` then report the served wall-clock window.",
        execution(task_support = "optional")
    )]
    async fn delay_read(
        &self,
        Parameters(args): Parameters<DelayArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let emit = args.emit_event.unwrap_or(false);
        let out = self.serve_delay(args).await;
        if emit {
            let _ = ctx.peer.notify_tool_list_changed().await;
        }
        Ok(out)
    }

    /// Write delay probe (sidecar marks it write → GraphQL `Mutation`).
    #[tool(description = "Sleep `ms` then report the served wall-clock window (write).")]
    async fn delay_write(
        &self,
        Parameters(args): Parameters<DelayArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let emit = args.emit_event.unwrap_or(false);
        let out = self.serve_delay(args).await;
        if emit {
            let _ = ctx.peer.notify_tool_list_changed().await;
        }
        Ok(out)
    }
}

#[tool_handler]
impl ServerHandler for MockDelay {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mock-delay-upstream", "0.0.0"))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let label = std::env::var("MOCK_LABEL").unwrap_or_else(|_| "mock".to_string());
    let server = MockDelay::new(label);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
