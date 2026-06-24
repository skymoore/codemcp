//! Shared, mutable gateway runtime.
//!
//! Holds the connected upstreams, the boot-time config (so a disabled server can
//! still be connected on demand), the executor (Python worker), and the current
//! SDK/tool-description state. The admin interface mutates this at runtime:
//! enabling/disabling an upstream reconnects it, regenerates the SDK, hot-reloads
//! the worker, and notifies MCP clients that the tool list changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::service::{Peer, RoleServer};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::auth::{self, AuthStartResult, AuthStatus, LoginHandle};
use crate::config::{self, ServerSpec};
use crate::env::Isolation;
use crate::error::Error;
use crate::exec::Executor;
use crate::launcher::Launcher;
use crate::prompt;
use crate::sdk::summary;
use crate::sdk::SdkRegistry;
use crate::upstream::SharedUpstreams;

/// The current generated SDK + its `execute_python` description.
pub struct SdkState {
    pub registry: SdkRegistry,
    pub description: String,
}

/// Status of one configured server, for `list`.
#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub name: String,
    pub kind: String,
    pub enabled_in_config: bool,
    pub connected: bool,
    pub tools: usize,
    /// OAuth authentication status (for remote servers).
    pub auth_status: String,
    /// Hint message for the user (e.g. "Run: codemcp auth <name>").
    pub auth_hint: Option<String>,
}

/// Status of one tool, for `tools` / the TUI.
///
/// - `enabled` is the *effective* state (session override wins, then the
///   configured default, then `true`).
/// - `default` is the persisted default from `mcp.json` (`true` if unset).
/// - `session` is the in-memory override for this gateway run, if any.
/// - `connected` is whether the owning server is currently connected (a tool
///   may be listed with `connected: false` when it has a configured default but
///   its server is down, or the upstream no longer advertises it).
#[derive(Debug, Serialize, Clone)]
pub struct ToolStatus {
    pub server: String,
    pub tool: String,
    pub summary: String,
    pub connected: bool,
    pub enabled: bool,
    pub default: bool,
    pub session: Option<bool>,
}

/// The shared runtime. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

struct Inner {
    upstreams: SharedUpstreams,
    executor: Arc<dyn Executor>,
    isolation: Isolation,
    config_path: PathBuf,
    launcher: Launcher,
    /// Boot config: every server (enabled or not), interpolated.
    boot: Mutex<BTreeMap<String, ConfigEntry>>,
    /// Current SDK + description, regenerated on every change.
    sdk: Mutex<SdkState>,
    /// Connected MCP client peers to notify on tool-list changes.
    peers: Mutex<Vec<Peer<RoleServer>>>,
    /// Persisted per-tool default `enabled` flags, keyed by (server, tool).
    /// Seeded from `mcp.json` at boot and updated on `--make-default`.
    tool_defaults: Mutex<BTreeMap<(String, String), bool>>,
    /// In-memory per-tool session overrides for this gateway run, keyed by
    /// (server, tool). Wins over `tool_defaults`. Cleared on restart.
    tool_session: Mutex<BTreeMap<(String, String), bool>>,
    /// Pending OAuth login flows, keyed by server name. Populated by
    /// `auth_start`, consumed by `auth_finish`.
    pending_oauth: Mutex<HashMap<String, LoginHandle>>,
}

#[derive(Clone)]
struct ConfigEntry {
    spec: ServerSpec,
    enabled: bool,
}

impl Runtime {
    pub async fn new(
        upstreams: SharedUpstreams,
        executor: Arc<dyn Executor>,
        isolation: Isolation,
        config_path: PathBuf,
        launcher: Launcher,
        sdk: SdkState,
    ) -> Result<Self, Error> {
        let boot_list = config::load_all(&config_path)?;
        let mut boot = BTreeMap::new();
        let mut tool_defaults = BTreeMap::new();
        for c in boot_list {
            for (tool, en) in &c.tool_defaults {
                tool_defaults.insert((c.name.clone(), tool.clone()), *en);
            }
            boot.insert(
                c.name,
                ConfigEntry {
                    spec: c.spec,
                    enabled: c.enabled,
                },
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                upstreams,
                executor,
                isolation,
                config_path,
                launcher,
                boot: Mutex::new(boot),
                sdk: Mutex::new(sdk),
                peers: Mutex::new(Vec::new()),
                tool_defaults: Mutex::new(tool_defaults),
                tool_session: Mutex::new(BTreeMap::new()),
                pending_oauth: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The config path this gateway was started with.
    pub fn config_path(&self) -> &std::path::Path {
        &self.inner.config_path
    }

    /// The application that launched this gateway.
    pub fn launcher(&self) -> &Launcher {
        &self.inner.launcher
    }

    /// Current `execute_python` description (clone).
    pub async fn description(&self) -> String {
        self.inner.sdk.lock().await.description.clone()
    }

    /// The full MCP `Tool` object the model sees in `tools/list`: name,
    /// description, and the fixed `{code: string}` input schema.
    pub async fn tool_definition(&self) -> serde_json::Value {
        use serde_json::{json, Map};
        let schema: Map<String, serde_json::Value> = json!({
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
        json!({
            "name": "execute_python",
            "description": self.description().await,
            "inputSchema": schema,
        })
    }

    /// Current generated `sdk.py` source (clone).
    pub async fn sdk_py(&self) -> String {
        self.inner.sdk.lock().await.registry.generate_sdk_py()
    }

    /// Run user code in the Python worker.
    pub async fn executor_run(&self, code: String) -> Result<crate::control::RunOutput, Error> {
        self.inner.executor.run(code).await
    }

    /// Register a connected MCP client peer (for tool-list-changed notifications).
    pub async fn register_peer(&self, peer: Peer<RoleServer>) {
        self.inner.peers.lock().await.push(peer);
    }

    /// Status of every configured server.
    pub async fn list(&self) -> Vec<ServerStatus> {
        let (session, defaults) = {
            let s = self.inner.tool_session.lock().await;
            let d = self.inner.tool_defaults.lock().await;
            (s.clone(), d.clone())
        };
        let effective = |server: &str, tool: &str| -> bool {
            let k = (server.to_string(), tool.to_string());
            session
                .get(&k)
                .copied()
                .or_else(|| defaults.get(&k).copied())
                .unwrap_or(true)
        };

        let boot = self.inner.boot.lock().await;
        let mut out = Vec::new();
        for (name, entry) in boot.iter() {
            let connected = self.inner.upstreams.is_connected(name).await;
            let tools = if connected {
                self.inner
                    .upstreams
                    .all_tools()
                    .await
                    .iter()
                    .filter(|t| t.server == *name && effective(&t.server, t.tool.name.as_ref()))
                    .count()
            } else {
                0
            };
            let kind = match entry.spec {
                ServerSpec::Local { .. } => "local",
                ServerSpec::Remote { .. } => "remote",
            }
            .to_string();

            let auth_status = if matches!(entry.spec, ServerSpec::Local { .. }) {
                AuthStatus::NotApplicable
            } else if connected {
                AuthStatus::Authenticated
            } else {
                self.inner.upstreams.auth_status(name).await
            };

            let auth_hint = if auth_status == AuthStatus::NeedsAuth {
                Some(format!("Run: codemcp auth {name}"))
            } else {
                None
            };

            out.push(ServerStatus {
                name: name.clone(),
                kind,
                enabled_in_config: entry.enabled,
                connected,
                tools,
                auth_status: auth_status.as_str().to_string(),
                auth_hint,
            });
        }
        out
    }

    /// Enable (connect) a server at runtime. Returns the number of tools it
    /// exposes. When `make_default`, also persists `enabled: true` to the config.
    pub async fn enable(&self, name: &str, make_default: bool) -> Result<usize, Error> {
        let spec = {
            let boot = self.inner.boot.lock().await;
            boot.get(name)
                .map(|e| e.spec.clone())
                .ok_or_else(|| Error::Config(format!("unknown server: {name}")))?
        };

        let (count, _) = self.inner.upstreams.connect_one(name, &spec).await?;
        self.regenerate_and_reload().await?;

        if make_default {
            config::set_enabled(&self.inner.config_path, name, true)?;
            if let Some(e) = self.inner.boot.lock().await.get_mut(name) {
                e.enabled = true;
            }
        }
        Ok(count)
    }

    /// Disable (disconnect) a server at runtime. When `make_default`, also
    /// persists `enabled: false` to the config.
    pub async fn disable(&self, name: &str, make_default: bool) -> Result<bool, Error> {
        {
            let boot = self.inner.boot.lock().await;
            if !boot.contains_key(name) {
                return Err(Error::Config(format!("unknown server: {name}")));
            }
        }
        let was = self.inner.upstreams.disconnect_one(name).await;
        self.regenerate_and_reload().await?;

        if make_default {
            config::set_enabled(&self.inner.config_path, name, false)?;
            if let Some(e) = self.inner.boot.lock().await.get_mut(name) {
                e.enabled = false;
            }
        }
        Ok(was)
    }

    /// Rebuild the SDK from currently-connected tools, hot-reload the worker, and
    /// notify MCP clients that the tool list changed.
    async fn regenerate_and_reload(&self) -> Result<(), Error> {
        // Snapshot the tool-state maps so we don't hold their locks across the
        // worker reload / peer notifications.
        let (session, defaults) = {
            let s = self.inner.tool_session.lock().await;
            let d = self.inner.tool_defaults.lock().await;
            (s.clone(), d.clone())
        };
        let effective = |server: &str, tool: &str| -> bool {
            let k = (server.to_string(), tool.to_string());
            session
                .get(&k)
                .copied()
                .or_else(|| defaults.get(&k).copied())
                .unwrap_or(true)
        };

        let tools = self.inner.upstreams.all_tools().await;
        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|nt| effective(&nt.server, nt.tool.name.as_ref()))
            .collect();
        let registry = SdkRegistry::build(&filtered);
        let sdk_py = registry.generate_sdk_py();
        let description = prompt::build_description(&registry, self.inner.isolation);

        // Hot-reload the worker's SDK module.
        self.inner.executor.reload_sdk(&sdk_py).await?;

        // Swap the shared SDK state.
        {
            let mut sdk = self.inner.sdk.lock().await;
            sdk.registry = registry;
            sdk.description = description;
        }

        // Notify clients; drop peers whose connection has gone away.
        let mut peers = self.inner.peers.lock().await;
        let mut alive = Vec::with_capacity(peers.len());
        for peer in peers.drain(..) {
            if peer.notify_tool_list_changed().await.is_ok() {
                alive.push(peer);
            }
        }
        *peers = alive;

        Ok(())
    }

    /// The effective enabled state for one tool: a session override wins, then
    /// the configured default, then `true` (tools are on unless told otherwise).
    async fn tool_effective_enabled(&self, server: &str, tool: &str) -> bool {
        let k = (server.to_string(), tool.to_string());
        {
            let s = self.inner.tool_session.lock().await;
            if let Some(v) = s.get(&k) {
                return *v;
            }
        }
        let d = self.inner.tool_defaults.lock().await;
        d.get(&k).copied().unwrap_or(true)
    }

    /// Enable a single tool at runtime. Auto-connects the server if it is not
    /// already connected, sets a session override to `on`, and (when
    /// `make_default`) persists `tools.<tool>.enabled: true` to `mcp.json`.
    /// Returns the resulting effective enabled state.
    pub async fn enable_tool(
        &self,
        server: &str,
        tool: &str,
        make_default: bool,
    ) -> Result<bool, Error> {
        if !self.inner.upstreams.is_connected(server).await {
            let spec = {
                let boot = self.inner.boot.lock().await;
                boot.get(server)
                    .map(|e| e.spec.clone())
                    .ok_or_else(|| Error::Config(format!("unknown server: {server}")))?
            };
            let _ = self.inner.upstreams.connect_one(server, &spec).await?;
        }

        {
            let mut s = self.inner.tool_session.lock().await;
            s.insert((server.to_string(), tool.to_string()), true);
        }
        if make_default {
            config::set_tool_enabled(&self.inner.config_path, server, tool, true)?;
            let mut d = self.inner.tool_defaults.lock().await;
            d.insert((server.to_string(), tool.to_string()), true);
        }

        self.regenerate_and_reload().await?;
        Ok(self.tool_effective_enabled(server, tool).await)
    }

    /// Disable a single tool at runtime. Sets a session override to `off` (the
    /// server stays connected so its other tools keep working) and (when
    /// `make_default`) persists `tools.<tool>.enabled: false` to `mcp.json`.
    /// The server need not be connected (a pre-disable applies on next connect).
    /// Returns the resulting effective enabled state.
    pub async fn disable_tool(
        &self,
        server: &str,
        tool: &str,
        make_default: bool,
    ) -> Result<bool, Error> {
        {
            let boot = self.inner.boot.lock().await;
            if !boot.contains_key(server) {
                return Err(Error::Config(format!("unknown server: {server}")));
            }
        }

        {
            let mut s = self.inner.tool_session.lock().await;
            s.insert((server.to_string(), tool.to_string()), false);
        }
        if make_default {
            config::set_tool_enabled(&self.inner.config_path, server, tool, false)?;
            let mut d = self.inner.tool_defaults.lock().await;
            d.insert((server.to_string(), tool.to_string()), false);
        }

        self.regenerate_and_reload().await?;
        Ok(self.tool_effective_enabled(server, tool).await)
    }

    /// Status of every known tool across all configured servers.
    ///
    /// Connected servers contribute their advertised tools (with summaries).
    /// Additionally, any tool with a configured default that is not currently
    /// advertised (server down, or the upstream dropped/renamed it) is included
    /// with `connected` reflecting the server's live state and an empty summary.
    pub async fn list_tools(&self) -> Vec<ToolStatus> {
        let (session, defaults) = {
            let s = self.inner.tool_session.lock().await;
            let d = self.inner.tool_defaults.lock().await;
            (s.clone(), d.clone())
        };

        let all = self.inner.upstreams.all_tools().await;
        let mut added: BTreeSet<(String, String)> = BTreeSet::new();
        let mut out = Vec::new();

        for nt in &all {
            let tool_name = nt.tool.name.as_ref().to_string();
            let k = (nt.server.clone(), tool_name.clone());
            let default = defaults.get(&k).copied().unwrap_or(true);
            let sess = session.get(&k).copied();
            out.push(ToolStatus {
                server: nt.server.clone(),
                tool: tool_name,
                summary: summary::from_description(&nt.tool),
                connected: true,
                enabled: sess.unwrap_or(default),
                default,
                session: sess,
            });
            added.insert(k);
        }

        // Configured-default tools not currently advertised (server down, or
        // upstream no longer lists them). Surfaces persisted defaults so the TUI
        // can show/flip them even when the server is disconnected.
        for ((srv, tool), default) in &defaults {
            let k = (srv.clone(), tool.clone());
            if added.contains(&k) {
                continue;
            }
            let connected = self.inner.upstreams.is_connected(srv).await;
            let sess = session.get(&k).copied();
            out.push(ToolStatus {
                server: srv.clone(),
                tool: tool.clone(),
                summary: String::new(),
                connected,
                enabled: sess.unwrap_or(*default),
                default: *default,
                session: sess,
            });
            added.insert(k);
        }

        out.sort_by(|a, b| (&a.server, &a.tool).cmp(&(&b.server, &b.tool)));
        out
    }

    // --- OAuth authentication ------------------------------------------------

    /// Start an OAuth authorization flow for a remote server.
    ///
    /// Discovers OAuth metadata, registers the client, starts a localhost
    /// callback server, and returns the authorization URL. The pending flow is
    /// stored internally; call `auth_finish` to complete it.
    pub async fn auth_start(&self, name: &str) -> Result<AuthStartResult, Error> {
        let spec = {
            let boot = self.inner.boot.lock().await;
            boot.get(name)
                .map(|e| e.spec.clone())
                .ok_or_else(|| Error::Config(format!("unknown server: {name}")))?
        };

        let (url, oauth_config) = match &spec {
            ServerSpec::Remote { url, oauth, .. } => {
                if oauth.as_ref().is_some_and(|o| o.is_disabled()) {
                    return Err(Error::Config(format!(
                        "{name}: OAuth is explicitly disabled in config"
                    )));
                }
                (
                    url.clone(),
                    oauth.as_ref().and_then(|o| o.config()).cloned(),
                )
            }
            ServerSpec::Local { .. } => {
                return Err(Error::Config(format!(
                    "{name}: OAuth is only for remote servers"
                )));
            }
        };

        let (result, handle) = auth::login::start(name, &url, oauth_config.as_ref())
            .await
            .map_err(|e| Error::Upstream(format!("{name}: OAuth start failed: {e}")))?;

        self.inner
            .pending_oauth
            .lock()
            .await
            .insert(name.to_string(), handle);

        Ok(result)
    }

    /// Finish a pending OAuth flow: wait for the browser callback, exchange the
    /// authorization code for tokens (auto-saved to `mcp-auth.json`), then
    /// reconnect the upstream with the new credentials.
    ///
    /// Returns the number of tools the now-authenticated server exposes.
    pub async fn auth_finish(&self, name: &str) -> Result<usize, Error> {
        let handle = {
            let mut pending = self.inner.pending_oauth.lock().await;
            pending
                .remove(name)
                .ok_or_else(|| Error::Config(format!("{name}: no pending OAuth flow")))?
        };

        // Wait for the callback and exchange the code for tokens.
        auth::login::finish(handle)
            .await
            .map_err(|e| Error::Upstream(format!("{name}: OAuth finish failed: {e}")))?;

        // Reconnect the upstream with the new tokens.
        let spec = {
            let boot = self.inner.boot.lock().await;
            boot.get(name)
                .map(|e| e.spec.clone())
                .ok_or_else(|| Error::Config(format!("unknown server: {name}")))?
        };

        let (count, _auth_status) = self.inner.upstreams.connect_one(name, &spec).await?;
        self.regenerate_and_reload().await?;
        Ok(count)
    }

    /// Remove stored OAuth credentials for a server and disconnect it.
    pub async fn auth_remove(&self, name: &str) -> Result<bool, Error> {
        // Cancel any pending flow.
        if let Some(handle) = self.inner.pending_oauth.lock().await.remove(name) {
            auth::login::cancel(handle);
        }

        // Remove stored tokens.
        let existed = auth::store::remove_tokens(name)
            .map_err(|e| Error::Config(format!("failed to remove credentials: {e}")))?;

        // Disconnect the upstream.
        self.inner.upstreams.disconnect_one(name).await;
        self.regenerate_and_reload().await?;

        Ok(existed)
    }

    /// Get the auth status for a specific server.
    #[allow(dead_code)]
    pub async fn auth_status(&self, name: &str) -> AuthStatus {
        self.inner.upstreams.auth_status(name).await
    }

    pub async fn shutdown(&self) {
        self.inner.executor.shutdown().await;
        self.inner.upstreams.shutdown().await;
    }
}
