//! Admin interface: a Unix-domain-socket JSON-RPC channel for mutating the live
//! gateway's connected upstream set without restarting it.
//!
//! Line-delimited JSON: the client sends one request object, the server replies
//! with one response object, then the connection closes. Methods:
//!   - `list`    -> `{ servers: [ServerStatus] }`
//!   - `enable`  { name, make_default? } -> `{ name, connected, tools }`
//!   - `disable` { name, make_default? } -> `{ name, connected }`
//!   - `tools`        -> `{ tools: [ToolStatus] }`
//!   - `enable_tool`  { server, tool, make_default? } -> `{ server, tool, enabled, made_default }`
//!   - `disable_tool` { server, tool, make_default? } -> `{ server, tool, enabled, made_default }`

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::error::Error;
use crate::runtime::Runtime;

/// Directory that holds per-instance admin sockets.
fn socket_dir() -> PathBuf {
    crate::env::config_base().join("codemcp")
}

/// Filename prefix for discoverable per-instance admin sockets.
const SOCKET_PREFIX: &str = "admin-";

/// A short, stable hash of the config path so sockets for different configs are
/// distinguishable at a glance.
fn config_hash(config_path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    // Canonicalize when possible so equivalent paths map to the same hash.
    let canon = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:08x}", (h.finish() as u32))
}

/// Resolve the admin socket path for *this* running gateway.
///
/// `CODEMCP_ADMIN_SOCKET` overrides everything (explicit path). Otherwise the
/// socket is per-instance: `admin-<config-hash>-<pid>.sock`, so two gateways
/// (e.g. one per harness) never collide and each is independently addressable.
pub fn socket_path(config_path: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("CODEMCP_ADMIN_SOCKET") {
        return PathBuf::from(p);
    }
    let name = format!(
        "{SOCKET_PREFIX}{}-{}.sock",
        config_hash(config_path),
        std::process::id()
    );
    socket_dir().join(name)
}

/// Discover all candidate admin sockets in the socket directory (plus an
/// explicit `CODEMCP_ADMIN_SOCKET` if set). Does not check liveness.
pub fn discover_sockets() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("CODEMCP_ADMIN_SOCKET") {
        out.push(PathBuf::from(p));
        return out;
    }
    if let Ok(entries) = std::fs::read_dir(socket_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(SOCKET_PREFIX) && name.ends_with(".sock") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

#[derive(Debug, Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnableParams {
    pub name: String,
    #[serde(default)]
    pub make_default: bool,
}

/// Params for the tool-level `enable_tool` / `disable_tool` methods.
#[derive(Debug, Deserialize, Serialize)]
pub struct ToolParams {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub make_default: bool,
}

/// Params for the auth methods (`auth_start`, `auth_finish`, `auth_remove`).
#[derive(Debug, Deserialize, Serialize)]
pub struct NameParams {
    pub name: String,
}

/// Start the admin Unix-socket server on this instance's per-instance socket
/// path, sets 0600 perms, and serves requests until the process exits.
pub async fn serve(runtime: Runtime) -> Result<(), Error> {
    let path = socket_path(runtime.config_path());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The path includes our PID, so any file here is a stale leftover from a
    // crashed process that reused the PID; safe to remove.
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let listener = UnixListener::bind(&path)
        .map_err(|e| Error::Other(format!("admin socket bind {} failed: {e}", path.display())))?;
    set_perms(&path);
    tracing::info!(socket = %path.display(), "admin interface listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let rt = runtime.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, rt).await {
                        tracing::warn!(error = %e, "admin connection error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "admin accept failed");
            }
        }
    }
}

#[cfg(unix)]
fn set_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path) {}

async fn handle_conn(stream: UnixStream, runtime: Runtime) -> Result<(), Error> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let response = match serde_json::from_str::<Request>(&line) {
        Ok(req) => dispatch(&runtime, req).await,
        Err(e) => json!({ "error": format!("invalid request: {e}") }),
    };

    let mut out = serde_json::to_string(&response)?;
    out.push('\n');
    reader.get_mut().write_all(out.as_bytes()).await?;
    reader.get_mut().flush().await?;
    Ok(())
}

async fn dispatch(runtime: &Runtime, req: Request) -> Value {
    match req.method.as_str() {
        // Lightweight liveness + identity probe used by client-side discovery.
        "info" => {
            let l = runtime.launcher();
            json!({
                "pid": std::process::id(),
                "config": runtime.config_path().display().to_string(),
                "launcher": l.name,
                "launcher_source": l.source,
                "parent_pid": l.parent_pid,
            })
        }
        "list" => {
            let servers = runtime.list().await;
            json!({ "servers": servers })
        }
        "tool" => {
            json!({ "tool": runtime.tool_definition().await })
        }
        "sdk" => {
            json!({ "sdk": runtime.sdk_py().await })
        }
        "enable" => match serde_json::from_value::<EnableParams>(req.params) {
            Ok(p) => match runtime.enable(&p.name, p.make_default).await {
                Ok(tools) => json!({
                    "name": p.name,
                    "connected": true,
                    "tools": tools,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "disable" => match serde_json::from_value::<EnableParams>(req.params) {
            Ok(p) => match runtime.disable(&p.name, p.make_default).await {
                Ok(was) => json!({
                    "name": p.name,
                    "connected": false,
                    "was_connected": was,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "tools" => {
            let tools = runtime.list_tools().await;
            json!({ "tools": tools })
        }
        "enable_tool" => match serde_json::from_value::<ToolParams>(req.params) {
            Ok(p) => match runtime
                .enable_tool(&p.server, &p.tool, p.make_default)
                .await
            {
                Ok(enabled) => json!({
                    "server": p.server,
                    "tool": p.tool,
                    "enabled": enabled,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "disable_tool" => match serde_json::from_value::<ToolParams>(req.params) {
            Ok(p) => match runtime
                .disable_tool(&p.server, &p.tool, p.make_default)
                .await
            {
                Ok(enabled) => json!({
                    "server": p.server,
                    "tool": p.tool,
                    "enabled": enabled,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        // --- OAuth authentication ---
        "auth_start" => match serde_json::from_value::<NameParams>(req.params) {
            Ok(p) => match runtime.auth_start(&p.name).await {
                Ok(result) => json!({
                    "authorization_url": result.authorization_url,
                    "oauth_state": result.oauth_state,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "auth_finish" => match serde_json::from_value::<NameParams>(req.params) {
            Ok(p) => match runtime.auth_finish(&p.name).await {
                Ok(tools) => json!({
                    "name": p.name,
                    "connected": true,
                    "tools": tools,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "auth_remove" => match serde_json::from_value::<NameParams>(req.params) {
            Ok(p) => match runtime.auth_remove(&p.name).await {
                Ok(existed) => json!({
                    "name": p.name,
                    "removed": existed,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "auth_status" => {
            let servers = runtime.list().await;
            json!({ "servers": servers })
        }
        other => json!({ "error": format!("unknown method: {other}") }),
    }
}

/// A discovered, live gateway instance.
#[derive(Debug, Clone)]
pub struct Instance {
    pub socket: PathBuf,
    pub pid: u64,
    pub config: String,
    /// Friendly launcher name (label or detected parent), e.g. `opencode`.
    pub launcher: String,
}

/// Send one request to a specific admin socket and return the response.
async fn request_on(path: &Path, method: &str, params: Value) -> Result<Value, Error> {
    let stream = UnixStream::connect(path).await.map_err(|e| {
        Error::Other(format!(
            "cannot reach codemcp admin socket at {} ({e}). Is the gateway running?",
            path.display()
        ))
    })?;

    let mut reader = BufReader::new(stream);
    let mut req = serde_json::to_string(&json!({ "method": method, "params": params }))?;
    req.push('\n');
    reader.get_mut().write_all(req.as_bytes()).await?;
    reader.get_mut().flush().await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp: Value = serde_json::from_str(&line)
        .map_err(|e| Error::Other(format!("invalid admin response: {e}")))?;
    Ok(resp)
}

/// Probe all discovered sockets and return the ones that answer `info` (live).
pub async fn live_instances() -> Vec<Instance> {
    let mut out = Vec::new();
    for socket in discover_sockets() {
        match request_on(&socket, "info", json!({})).await {
            Ok(resp) => {
                let pid = resp.get("pid").and_then(Value::as_u64).unwrap_or(0);
                let config = resp
                    .get("config")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let launcher = resp
                    .get("launcher")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                out.push(Instance {
                    socket,
                    pid,
                    config,
                    launcher,
                });
            }
            // Dead/stale socket: skip it.
            Err(_) => continue,
        }
    }
    out
}

/// Pick the target instance for a client command.
///
/// `selector` (from `--instance`) matches against the launcher name or config
/// path (substring) or an exact PID. With no selector: if exactly one instance
/// is live, use it; if several, return an error listing them to disambiguate.
pub async fn select_instance(selector: Option<&str>) -> Result<Instance, Error> {
    let mut instances = live_instances().await;

    if let Some(sel) = selector {
        instances.retain(|i| {
            i.launcher.contains(sel) || i.config.contains(sel) || i.pid.to_string() == sel
        });
        match instances.len() {
            0 => Err(Error::Other(format!(
                "no running codemcp gateway matches --instance {sel:?}"
            ))),
            1 => Ok(instances.remove(0)),
            _ => Err(Error::Other(format!(
                "--instance {sel:?} is ambiguous; matches:\n{}",
                format_instances(&instances)
            ))),
        }
    } else {
        match instances.len() {
            0 => Err(Error::Other(
                "no running codemcp gateway found. Is one started? \
                 (a gateway runs when a harness launches codemcp, or via `codemcp start`)"
                    .to_string(),
            )),
            1 => Ok(instances.remove(0)),
            _ => Err(Error::Other(format!(
                "multiple codemcp gateways are running; pass --instance <launcher|config-substring|pid>:\n{}",
                format_instances(&instances)
            ))),
        }
    }
}

fn format_instances(instances: &[Instance]) -> String {
    instances
        .iter()
        .map(|i| format!("  [{}]  pid {}  config {}", i.launcher, i.pid, i.config))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Client side: send one request to the selected gateway's admin socket.
pub async fn client_request(
    selector: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    let instance = select_instance(selector).await?;
    request_on(&instance.socket, method, params).await
}

/// Client side: send one request to a specific (already-selected) gateway's
/// admin socket. Used by the TUI, which selects an instance once and then
/// issues many requests against it without re-discovering each time.
#[allow(dead_code)]
pub async fn client_request_on(socket: &Path, method: &str, params: Value) -> Result<Value, Error> {
    request_on(socket, method, params).await
}
