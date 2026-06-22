//! Manages the set of connected upstream MCP servers.
//!
//! Connects to all enabled servers concurrently at startup, lists their tools,
//! and routes tool calls to the owning server. Failed upstreams are logged and
//! skipped (never fatal).

mod client;

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};

use crate::config::UpstreamConfig;
use crate::error::Error;

use client::UpstreamService;

/// One connected upstream: its live service plus the tools it exposes.
struct Upstream {
    service: UpstreamService,
    tools: Vec<Tool>,
}

/// A tool exposed by some upstream, tagged with its server.
#[derive(Debug, Clone)]
pub struct NamespacedTool {
    pub server: String,
    pub tool: Tool,
}

/// Holds all upstream connections and provides tool routing.
pub struct UpstreamManager {
    upstreams: HashMap<String, Upstream>,
}

impl UpstreamManager {
    /// Connect to all enabled upstreams concurrently. Servers that fail to
    /// connect are logged and omitted.
    pub async fn connect_all(configs: &[UpstreamConfig]) -> Self {
        let mut tasks = Vec::new();
        for cfg in configs {
            let cfg = cfg.clone();
            tasks.push(tokio::spawn(async move {
                let res = client::connect(&cfg.name, &cfg.spec).await;
                (cfg.name, res)
            }));
        }

        let mut upstreams = HashMap::new();
        for task in tasks {
            let (name, res) = match task.await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(error = %e, "upstream connect task panicked");
                    continue;
                }
            };
            match res {
                Ok(service) => {
                    let tools = match service.list_all_tools().await {
                        Ok(tools) => tools,
                        Err(e) => {
                            tracing::error!(server = %name, error = %e, "list_tools failed");
                            let _ = service.cancel().await;
                            continue;
                        }
                    };
                    tracing::info!(server = %name, tools = tools.len(), "connected upstream");
                    upstreams.insert(name, Upstream { service, tools });
                }
                Err(e) => {
                    tracing::error!(server = %name, error = %e, "failed to connect upstream");
                }
            }
        }

        Self { upstreams }
    }

    /// All tools across all upstreams, tagged with their server name.
    pub fn all_tools(&self) -> Vec<NamespacedTool> {
        let mut out = Vec::new();
        for (server, up) in &self.upstreams {
            for tool in &up.tools {
                out.push(NamespacedTool {
                    server: server.clone(),
                    tool: tool.clone(),
                });
            }
        }
        out.sort_by(|a, b| {
            (a.server.as_str(), a.tool.name.as_ref())
                .cmp(&(b.server.as_str(), b.tool.name.as_ref()))
        });
        out
    }

    /// Route a tool call to `server`'s upstream.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, Error> {
        let up = self
            .upstreams
            .get(server)
            .ok_or_else(|| Error::Upstream(format!("unknown upstream server: {server}")))?;

        let mut params = CallToolRequestParams::default();
        params.name = tool.to_string().into();
        params.arguments = arguments;

        up.service
            .call_tool(params)
            .await
            .map_err(|e| Error::Upstream(format!("{server}/{tool}: {e}")))
    }

    /// Gracefully disconnect every upstream.
    pub async fn shutdown(self) {
        for (name, up) in self.upstreams {
            if let Err(e) = up.service.cancel().await {
                tracing::warn!(server = %name, error = %e, "error during upstream shutdown");
            }
        }
    }
}

/// Convenience wrapper so the manager can be shared across tasks.
pub type SharedUpstreams = Arc<UpstreamManager>;
