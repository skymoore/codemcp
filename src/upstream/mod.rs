//! Manages the set of connected upstream MCP servers.
//!
//! Connects to enabled servers at startup, lists their tools, and routes tool
//! calls to the owning server. Failed upstreams are logged and skipped (never
//! fatal). Upstreams can be connected/disconnected at runtime via the admin
//! interface, so the set lives behind an `RwLock`.
//!
//! Remote servers that require OAuth are tracked with an `AuthStatus` so
//! `codemcp list` can inform the user when authentication is needed.

mod client;

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use tokio::sync::RwLock;

use crate::auth::AuthStatus;
use crate::config::{ServerSpec, UpstreamConfig};
use crate::error::Error;

// Re-exported so auth::oauth_client can use the same default timeout.
use client::UpstreamService;
pub(crate) use client::DEFAULT_CONNECT_TIMEOUT_SECS;

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

/// The outcome of connecting + listing tools for one upstream.
struct UpstreamOutcome {
    /// The connected upstream, if the connection succeeded.
    upstream: Option<Upstream>,
    /// Auth status for this server.
    auth_status: AuthStatus,
    /// Error message if the connection failed.
    error: Option<String>,
}

/// Holds all upstream connections and provides tool routing.
pub struct UpstreamManager {
    upstreams: RwLock<HashMap<String, Upstream>>,
    /// Auth status per server (persisted even when not connected).
    auth_status: RwLock<HashMap<String, AuthStatus>>,
}

impl UpstreamManager {
    /// Connect to all enabled upstreams concurrently. Servers that fail to
    /// connect are logged and omitted.
    pub async fn connect_all(configs: &[UpstreamConfig]) -> Self {
        let mut tasks = Vec::new();
        for cfg in configs {
            let name = cfg.name.clone();
            let spec = cfg.spec.clone();
            tasks.push(tokio::spawn(async move {
                let outcome = connect_and_list(&name, &spec).await;
                (name, outcome)
            }));
        }

        let mut upstreams = HashMap::new();
        let mut auth_status = HashMap::new();
        for task in tasks {
            let (name, outcome) = match task.await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(error = %e, "upstream connect task panicked");
                    continue;
                }
            };
            auth_status.insert(name.clone(), outcome.auth_status.clone());
            match outcome.upstream {
                Some(up) => {
                    tracing::info!(server = %name, tools = up.tools.len(), "connected upstream");
                    upstreams.insert(name, up);
                }
                None => {
                    if let Some(err) = &outcome.error {
                        tracing::error!(server = %name, error = %err, "failed to connect upstream");
                    }
                }
            }
        }

        Self {
            upstreams: RwLock::new(upstreams),
            auth_status: RwLock::new(auth_status),
        }
    }

    /// Connect a single upstream at runtime. Replaces any existing connection
    /// with the same name. Returns the number of tools and the auth status.
    pub async fn connect_one(
        &self,
        name: &str,
        spec: &ServerSpec,
    ) -> Result<(usize, AuthStatus), Error> {
        let outcome = connect_and_list(name, spec).await;
        let auth_status = outcome.auth_status.clone();

        // Record auth status regardless of connection outcome.
        self.auth_status
            .write()
            .await
            .insert(name.to_string(), auth_status.clone());

        match outcome.upstream {
            Some(up) => {
                let count = up.tools.len();
                let mut guard = self.upstreams.write().await;
                if let Some(old) = guard.insert(name.to_string(), up) {
                    let _ = old.service.cancel().await;
                }
                tracing::info!(server = %name, tools = count, "connected upstream (runtime)");
                Ok((count, auth_status))
            }
            None => {
                Err(Error::Upstream(outcome.error.unwrap_or_else(|| {
                    format!("unknown error connecting {name}")
                })))
            }
        }
    }

    /// Disconnect a single upstream at runtime. Returns true if it was connected.
    pub async fn disconnect_one(&self, name: &str) -> bool {
        let removed = { self.upstreams.write().await.remove(name) };
        match removed {
            Some(up) => {
                if let Err(e) = up.service.cancel().await {
                    tracing::warn!(server = %name, error = %e, "error disconnecting upstream");
                }
                tracing::info!(server = %name, "disconnected upstream (runtime)");
                true
            }
            None => false,
        }
    }

    /// Whether the named upstream is currently connected.
    pub async fn is_connected(&self, name: &str) -> bool {
        self.upstreams.read().await.contains_key(name)
    }

    /// Get the auth status for a server.
    #[allow(dead_code)]
    pub async fn auth_status(&self, name: &str) -> AuthStatus {
        self.auth_status
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or(AuthStatus::NotApplicable)
    }

    /// All tools across all connected upstreams, tagged with their server name.
    pub async fn all_tools(&self) -> Vec<NamespacedTool> {
        let guard = self.upstreams.read().await;
        let mut out = Vec::new();
        for (server, up) in guard.iter() {
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
        let guard = self.upstreams.read().await;
        let up = guard
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

    /// Gracefully disconnect every upstream (does not consume `self`).
    pub async fn shutdown(&self) {
        let drained: Vec<(String, Upstream)> = { self.upstreams.write().await.drain().collect() };
        for (name, up) in drained {
            if let Err(e) = up.service.cancel().await {
                tracing::warn!(server = %name, error = %e, "error during upstream shutdown");
            }
        }
    }
}

/// Connect to one upstream and list its tools.
async fn connect_and_list(name: &str, spec: &ServerSpec) -> UpstreamOutcome {
    let result = client::connect(name, spec).await;

    let service = match result.service {
        Some(s) => s,
        None => {
            return UpstreamOutcome {
                upstream: None,
                auth_status: result.auth_status,
                error: result.error,
            };
        }
    };

    let tools = match service.list_all_tools().await {
        Ok(tools) => tools,
        Err(e) => {
            let _ = service.cancel().await;
            return UpstreamOutcome {
                upstream: None,
                auth_status: result.auth_status,
                error: Some(format!("{name}: list_tools failed: {e}")),
            };
        }
    };
    UpstreamOutcome {
        upstream: Some(Upstream { service, tools }),
        auth_status: result.auth_status,
        error: None,
    }
}

/// Convenience wrapper so the manager can be shared across tasks.
pub type SharedUpstreams = Arc<UpstreamManager>;
