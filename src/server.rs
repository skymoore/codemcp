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
                                    assign to `result` or leave a final expression to return a value."
                },
                "allow_mutations": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "SDK function names for mutating (write) tools you \
                                    authorize this run to call, e.g. \
                                    [\"github_create_pull_request\"]. A write tool not \
                                    listed here is rejected before execution."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Preview mode. Mutating calls are NOT sent upstream — \
                                    they return a deterministic stub — while read calls \
                                    still execute. Returns the mutations that would occur."
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

/// Coerce a worker-provided value into a JSON array (worker may send null when a
/// run failed before producing a trace).
fn normalize_list(v: Value) -> Value {
    match v {
        Value::Array(_) => v,
        _ => Value::Array(Vec::new()),
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

        let args = request.arguments.as_ref();
        let code = args
            .and_then(|a| a.get("code"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required string argument `code`", None)
            })?
            .to_string();

        let allow_mutations = args
            .and_then(|a| a.get("allow_mutations"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let dry_run = args
            .and_then(|a| a.get("dry_run"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let opts = crate::control::RunOptions {
            allow_mutations,
            dry_run,
        };

        let out = self.runtime.executor_run(code, opts).await?;

        // Compact, always-present fields the harness can render/audit.
        let trace = normalize_list(out.trace);
        let mutations = normalize_list(out.mutations);

        // User code raised: surface as a tool error (structured), not a protocol
        // error — the agent can read the traceback and retry. Include the trace
        // so partial failures are localized to the exact call.
        if let Some(err) = out.error {
            return Ok(CallToolResult::structured_error(json!({
                "error": err,
                "stdout": out.stdout,
                "stderr": out.stderr,
                "trace": trace,
            })));
        }

        Ok(CallToolResult::structured(json!({
            "result": out.result,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "trace": trace,
            "mutations": mutations,
        })))
    }
}
