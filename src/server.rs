//! The downstream-facing MCP server.
//!
//! Exposes exactly one tool, `execute_python`, whose description carries the
//! currently-generated SDK signatures. Calls are forwarded to the [`Runtime`]'s
//! executor, which runs the user's Python (with the SDK preloaded) and returns
//! its result plus captured output. The description is dynamic: when upstreams
//! are enabled/disabled at runtime, the runtime regenerates it and the server
//! reflects the new value on the next `tools/list`.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorData, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{json, Map, Value};

use crate::runtime::Runtime;

const TOOL_NAME: &str = "execute_python";

/// The downstream MCP server. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct CodeServer {
    runtime: Runtime,
    input_schema: Arc<Map<String, Value>>,
}

impl CodeServer {
    pub fn new(runtime: Runtime) -> Self {
        let schema: Map<String, Value> = json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute. SDK functions are preloaded; \
                                    assign to `result` (or leave a final expression) to return a value."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .expect("schema is an object");

        Self {
            runtime,
            input_schema: Arc::new(schema),
        }
    }

    async fn tool(&self) -> Tool {
        Tool::new(
            Cow::Borrowed(TOOL_NAME),
            Cow::Owned(self.runtime.description().await),
            self.input_schema.clone(),
        )
    }
}

impl ServerHandler for CodeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "codemcp gateway: write Python that calls connected MCP tools as typed \
                 functions and returns a combined result in one step. See the \
                 `execute_python` tool description for the available SDK.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Capture the peer so runtime changes can push tools/list_changed.
        self.runtime.register_peer(context.peer.clone()).await;
        Ok(ListToolsResult {
            tools: vec![self.tool().await],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name != TOOL_NAME {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        }

        let code = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("code"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required string argument `code`", None)
            })?
            .to_string();

        let out = self.runtime.executor_run(code).await?;

        // User code raised: surface as a tool error (structured), not a protocol
        // error — the agent can read the traceback and retry.
        if let Some(err) = out.error {
            return Ok(CallToolResult::structured_error(json!({
                "error": err,
                "stdout": out.stdout,
                "stderr": out.stderr,
            })));
        }

        Ok(CallToolResult::structured(json!({
            "result": out.result,
            "stdout": out.stdout,
            "stderr": out.stderr,
        })))
    }
}
