//! Transparent MCP proxy handler.
//!
//! Exposes every resolved upstream tool 1:1 as an MCP tool, with names
//! prefixed `{server}__{tool}` to disambiguate across upstreams. No GraphQL,
//! no skills, no resources — pure passthrough of `tools/list` + `tools/call`.
//!
//! Mounted as a *side* endpoint by the bin when `[proxy] enabled = true`.
//! The GraphQL `VmcpServer` continues to serve `/mcp` in the same process.
//! Both share the same `UpstreamPool`, OAuth middleware, and recorder.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;
use tracing::warn;

use vmcp_upstream::UpstreamPool;

const NAME_SEP: &str = "__";

#[derive(Clone)]
pub struct ProxyServer {
    pool: Arc<UpstreamPool>,
}

impl ProxyServer {
    pub fn new(pool: Arc<UpstreamPool>) -> Self {
        Self { pool }
    }
}

impl ServerHandler for ProxyServer {
    fn get_info(&self) -> ServerInfo {
        let mut impl_info = Implementation::from_build_env();
        impl_info.name = "vmcp-proxy".into();
        impl_info.version = env!("CARGO_PKG_VERSION").into();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(impl_info)
            .with_instructions(
                "vmcp proxy: transparent passthrough of upstream MCP tools. \
                 Tool names are prefixed `{server}__{tool}` to disambiguate \
                 across upstreams. Use `tools/list` to discover, then call by \
                 the prefixed name.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let all = self.pool.all_resolved();
        let mut tools: Vec<Tool> = Vec::with_capacity(all.iter().map(|(_, v)| v.len()).sum());
        for (server, list) in all {
            let server_desc = self.pool.description_of(&server);
            for t in list {
                let prefixed = format!("{server}{NAME_SEP}{}", t.name);
                let description =
                    build_description(&server, server_desc.as_deref(), t.description.as_deref());
                let schema = into_schema_arc(&prefixed, &t.input_schema);
                tools.push(Tool::new_with_raw(
                    prefixed,
                    description.map(Into::into),
                    schema,
                ));
            }
        }
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let (server, tool) = request.name.split_once(NAME_SEP).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "tool name `{}` missing `{NAME_SEP}` prefix — expected `{{server}}{NAME_SEP}{{tool}}`",
                    request.name
                ),
                None,
            )
        })?;

        let args = match request.arguments {
            Some(obj) => Value::Object(obj),
            None => Value::Null,
        };

        self.pool
            .call(server, tool, args)
            .await
            .map_err(|e| McpError::internal_error(format!("upstream `{server}` call failed: {e}"), None))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        let (server, tool) = name.split_once(NAME_SEP)?;
        let resolved = self.pool.resolved(server)?;
        let t = resolved.into_iter().find(|t| t.name == tool)?;
        let server_desc = self.pool.description_of(server);
        let description = build_description(server, server_desc.as_deref(), t.description.as_deref());
        let schema = into_schema_arc(name, &t.input_schema);
        Some(Tool::new_with_raw(
            name.to_string(),
            description.map(Into::into),
            schema,
        ))
    }
}

fn build_description(
    server: &str,
    server_desc: Option<&str>,
    tool_desc: Option<&str>,
) -> Option<String> {
    Some(match (server_desc, tool_desc) {
        (Some(sd), Some(td)) => format!("[{server}] {sd} — {td}"),
        (Some(sd), None) => format!("[{server}] {sd}"),
        (None, Some(td)) => format!("[{server}] {td}"),
        (None, None) => format!("[{server}]"),
    })
}

fn into_schema_arc(name: &str, raw: &Value) -> Arc<JsonObject> {
    match raw.as_object() {
        Some(obj) => Arc::new(obj.clone()),
        None => {
            warn!(
                tool = %name,
                "upstream input_schema is not a JSON object, falling back to {{}}"
            );
            Arc::new(JsonObject::new())
        }
    }
}
